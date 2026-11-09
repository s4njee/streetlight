#!/usr/bin/env python3
"""Read-only per-minute process/thermal sampling; no GPU/CPU configuration changes."""
import json
import pathlib
import subprocess
import sys
import time
import os

parent=int(pathlib.Path(sys.argv[1]).read_text())
children=pathlib.Path(f'/proc/{parent}/task/{parent}/children').read_text().split()
if not children: raise SystemExit('No client process')
pid=int(children[0]); ticks=os.sysconf('SC_CLK_TCK')
start=time.monotonic(); last=start; last_cpu=None
while pathlib.Path(f'/proc/{pid}/stat').exists():
    try:
        fields=pathlib.Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()
        cpu=(int(fields[11])+int(fields[12]))/ticks
        now=time.monotonic()
        temp=int(pathlib.Path('/sys/class/thermal/thermal_zone0/temp').read_text())/1000
        result=dict(elapsed_s=round(now-start,2), pid=pid, temperature_c=temp,
                    cpu_percent=None if last_cpu is None else round((cpu-last_cpu)/(now-last)*100,2),
                    rss_mib=round(int(fields[21])*os.sysconf('SC_PAGE_SIZE')/2**20,2))
        result['throttled']=subprocess.check_output(['vcgencmd','get_throttled'],text=True).strip()
        print(json.dumps(result),flush=True)
        last_cpu=cpu; last=now
        for _ in range(60):
            if not pathlib.Path(f'/proc/{pid}/stat').exists(): break
            time.sleep(1)
    except FileNotFoundError: break
