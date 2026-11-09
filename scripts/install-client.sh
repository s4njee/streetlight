#!/usr/bin/env bash
# Install the built client for this user. Does not start it or change the current kiosk.
set -euo pipefail
cd "$(dirname "$0")/.."
install -Dm755 target/release/streetlight-client "$HOME/.local/bin/streetlight-client"
python3 - <<'PY'
import os, pathlib
home = pathlib.Path.home()
data = pathlib.Path(os.environ.get('XDG_DATA_HOME', home / '.local/share'))
config = pathlib.Path(os.environ.get('XDG_CONFIG_HOME', home / '.config'))
app = data / 'applications/streetlight-client.desktop'
unit = config / 'systemd/user/streetlight-client.service'
app.parent.mkdir(parents=True, exist_ok=True)
unit.parent.mkdir(parents=True, exist_ok=True)
command = str(home / '.local/bin/streetlight-client').replace('\\','\\\\').replace('"','\\"').replace('`','\\`').replace('$','\\$')
app.write_text(pathlib.Path('packaging/streetlight-client.desktop').read_text().replace('Exec=streetlight-client', f'Exec="{command}"'))
unit.write_text(pathlib.Path('packaging/streetlight-client.service').read_text())
print(f'Installed {app}\nInstalled optional {unit}')
PY
printf 'Launch: %s/.local/bin/streetlight-client\n' "$HOME"
printf 'The existing compositor/kiosk and startup settings are unchanged.\n'
