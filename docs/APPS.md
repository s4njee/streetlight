# Apps and stream lifecycle

The host app list supports multiple entries with an ID, label, shell command and
`stop_on_disconnect` policy. Clients select the ID from the app menu. Desktop has
no command. Unknown app IDs are rejected before the current stream is replaced.

```json
{
  "id": "my-game",
  "name": "My game",
  "command": "exec /path/to/game",
  "stop_on_disconnect": true
}
```

Each launch owns a new Unix process group. `stop_on_disconnect: true` sends TERM to
that group when its stream ends, permits up to 500 ms for exit, then sends KILL to
remaining members and reaps the direct child. `false` (the default) leaves it running
and reaps the direct child when it eventually exits. Only the group launched for
this stream is managed. A program that delegates to an existing daemon or escapes
its process group cannot be stopped through that group. The host never kills by
process name or adopts an already-running Steam instance.

The command receives `STREETLIGHT_OUTPUT` (the exact KWin connector, including
`Virtual-`), `STREETLIGHT_WIDTH` and `STREETLIGHT_HEIGHT`. These describe the stream;
an application still needs to place its window on that output. `/api/status` reports
`app_id`, `app_pid`, and whether the direct child is still running. App command
changes retain the host API's confirmation/token requirements.

## Steam Big Picture on the virtual output

KWin opens new windows on the active screen, which at the start of a stream is the
PC's own. While a stream runs an app with a command, the host loads a KWin script
(`crates/host/src/placement.rs`, through `org.kde.kwin.Scripting` on the session bus)
that sends each new normal window to the stream's output, keeps full screen, and
returns a window that goes full screen on another monitor. It is unloaded when the
stream ends; Desktop streams load nothing. So selecting **Steam Big Picture**
(`steam steam://open/bigpicture`, `stop_on_disconnect: false` for a shared Steam
instance) brings Big Picture, and the games it starts, onto the stream's monitor.

A Big Picture window already open before the stream isn't moved: close it first. A KWin
window rule on the stream's output (System Settings → Window Management → Window
Rules, property **Screen**) still works if you prefer fixed placement; the output's
name (`/api/status.status.output_name`, connector `Virtual-` + name) is stable per
client certificate and display name. The host never edits Plasma's window rules.
