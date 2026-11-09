#!/usr/bin/env python3
"""Watch only the test-created Streetlight pad, then require its removal."""
import errno, json, pathlib, select, sys, time
import evdev

deadline=time.monotonic()+35
device=None
while time.monotonic()<deadline:
    for path in evdev.list_devices():
        try: candidate=evdev.InputDevice(path)
        except OSError: continue
        if candidate.name=='Streetlight gamepad 0': device=candidate; break
        candidate.close()
    if device: break
    time.sleep(.02)
if device is None: raise SystemExit('FAIL: Streetlight gamepad never appeared')
print('PAD',device.path,device.name,flush=True)
seen=set(); removed=False
while time.monotonic()<deadline:
    if not pathlib.Path(device.path).exists(): removed=True; break
    try:
        if not select.select([device.fd],[],[],.1)[0]: continue
        for event in device.read():
            if event.type==evdev.ecodes.EV_KEY and event.code==evdev.ecodes.BTN_SOUTH:
                seen.add(('button',event.value)); print('BTN_SOUTH',event.value,flush=True)
            if event.type==evdev.ecodes.EV_ABS and event.code==evdev.ecodes.ABS_X:
                seen.add(('x',event.value)); print('ABS_X',event.value,flush=True)
    except OSError as e:
        if e.errno==errno.ENODEV: removed=True; break
        raise
if not removed or not {('button',1),('button',0),('x',20000),('x',0)} <= seen:
    raise SystemExit('FAIL: missing pad transitions/removal '+repr((seen,removed)))
print('PASS: pad press/release, stick, removal',flush=True)
