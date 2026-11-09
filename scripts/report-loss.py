#!/usr/bin/env python3
"""Summarize actual headless receiver stats; fail on missing evidence or regressions."""
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
print('| Impairment | Frames | Lost | FEC recovered | IDR requests | Keyframes | Dropped packets | Max gap ms | Max recovery ms | Decode errors |')
print('|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|')
failed = []
for mode in ['none', 'random2', 'random10', 'burst', 'reorder']:
    samples = []
    for line in (root / f'{mode}.log').read_text().splitlines():
        try:
            sample = json.loads(line)
            if 'frames' in sample:
                samples.append(sample)
        except (ValueError, TypeError):
            pass
    if len(samples) < 15:
        failed.append(f'{mode}: expected 15 one-second reports, got {len(samples)}')
    if not samples:
        continue
    s = samples[-1]
    errors = len((root / f'{mode}.decode.log').read_text().splitlines())
    print(f'| {mode} | {s["frames"]} | {s["frames_lost"]} | {s["frames_recovered"]} | {s["idr_requests"]} | {s["keyframes"]} | {s["test_packets_dropped"]} | {s["max_video_gap_ms"]:.2f} | {s["max_recovery_ms"]:.2f} | {errors} |')
    if errors or s['bad_packets'] or s['keyframes'] == 0:
        failed.append(f'{mode}: decoder/bad-packet/keyframe check failed')
    if mode in ('none', 'reorder') and s['frames_lost']:
        failed.append(f'{mode}: spurious loss')
    if mode == 'none' and (s['frames_recovered'] or s['test_packets_dropped']):
        failed.append('none: unexpected recovery/drop')
    if mode == 'random2' and (not s['frames_recovered'] or s['frames_lost'] > 2):
        failed.append('random2: expected FEC recovery and approximately zero frame loss')
    if mode != 'none' and not s['test_packets_dropped'] and mode != 'reorder':
        failed.append(f'{mode}: impairment did not run')
    if s['max_video_gap_ms'] > 500:
        failed.append(f'{mode}: stream froze for >500 ms')
    if mode == 'none' and s['frames'] < 825:
        failed.append('none: average fps below 55')
    if mode == 'random10' and s['max_recovery_ms'] > 1000 / 30 + s['rtt_ms']:
        failed.append('random10: loss-to-keyframe exceeded two frame intervals plus RTT')
if failed:
    print('\nFAILED:\n' + '\n'.join(failed), file=sys.stderr)
    sys.exit(1)
