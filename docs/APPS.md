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

1. Pair the normal client, start Desktop, and inspect `/api/status.status.output_name`.
   Its KWin connector is `Virtual-` followed by that value. The fingerprint-based
   name remains stable while that client's certificate and display name are kept.
2. Open Steam Big Picture. In Plasma System Settings → Window Management → Window
   Rules, add a rule and use **Detect Window Properties** on its window. Narrow the
   rule to the Big Picture window/application rather than all Steam game windows.
3. Add the **Screen** property and select the temporary Streetlight monitor. Use
   **Apply Initially** if you want to allow later manual movement; use **Force** if
   the application overrides the initial placement. Add Fullscreen if needed.
   KDE documents the [rule editor](https://docs.kde.org/stable_kf6/en/kwin/kcontrol/windowspecific/kwin-rule-editor.html)
   and [initial-placement examples](https://docs.kde.org/stable_kf6/en/kwin/kcontrol/windowspecific/examples.html).
4. Select the host's Steam Big Picture entry from the client app menu. Its existing
   command is `steam steam://open/bigpicture`. Keep `stop_on_disconnect: false` for
   a shared Steam instance. Verify placement once on your Plasma/Steam versions;
   the recipe has not launched or changed the user's Steam configuration.

Rules are configured by the host user; Streetlight does not silently edit Plasma's
rules or move existing windows. If the monitor list changes or the client certificate
is reset, revisit the rule's Screen selection.
