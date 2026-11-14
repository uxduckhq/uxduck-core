"""Use the selected Linux device node's capabilities, not its whole webcam's.

A UVC webcam can expose both video and metadata nodes with the same card name
and aggregate capabilities. device_open serves discovery and capture alike.
"""
from pathlib import Path
import sys

p = Path(sys.argv[1]) / 'libavdevice/v4l2.c'
s = p.read_text()
old = '''    av_log(ctx, AV_LOG_VERBOSE, "fd:%d capabilities:%x\\n",
           fd, cap.capabilities);'''
new = '''    // capabilities describes the whole physical device; device_caps describes
    // this node. In particular, a webcam's metadata node cannot supply pictures.
    if (cap.capabilities & V4L2_CAP_DEVICE_CAPS)
        cap.capabilities = cap.device_caps;

''' + old
if s.count(old) != 1:
    raise SystemExit('FFmpeg source changed: expected one device_open capability check')
p.write_text(s.replace(old, new))
