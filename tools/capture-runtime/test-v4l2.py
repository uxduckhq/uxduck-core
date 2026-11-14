"""Exercise FFmpeg's real device_open with synthetic Linux capability replies.

No camera is opened or filmed. Only OS calls are replaced; the function below
is extracted verbatim from the source used to build the capture executable.
"""
from pathlib import Path
import subprocess
import sys
import tempfile

ffmpeg = Path(sys.argv[1]).resolve()
source = (ffmpeg / 'libavdevice/v4l2.c').read_text()
start = source.index('static int device_open(')
end = source.index('\nstatic int device_init(', start)
setup = source[start:end]
harness = r'''
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <time.h>
#include <linux/videodev2.h>
#include <stdarg.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <unistd.h>

typedef struct video_data {
    int use_libv4l2, multiplanar, buf_type;
    int (*open_f)(const char *, int, ...);
    int (*close_f)(int);
    int (*dup_f)(int);
    int (*ioctl_f)(int, unsigned long, ...);
    ssize_t (*read_f)(int, void *, size_t);
    void *(*mmap_f)(void *, size_t, int, int, int, off_t);
    int (*munmap_f)(void *, size_t);
} video_data;
typedef struct { video_data *priv_data; int flags; } AVFormatContext;
#define CONFIG_LIBV4L2 0
#define AVFMT_FLAG_NONBLOCK 4
#define AVERROR(e) (-(e))
#define av_log(...) ((void)0)

static struct v4l2_capability reply;
static int opens, closes, query_error;
static int fake_open(const char *path, int flags, ...) {
    assert(!strcmp(path, "/dev/video-fixture"));
    assert(flags == O_RDWR);
    ++opens;
    return 17;
}
static int fake_close(int fd) { assert(fd == 17); ++closes; return 0; }
static int fake_ioctl(int fd, unsigned long request, ...) {
    assert(fd == 17 && request == VIDIOC_QUERYCAP);
    if (query_error) { errno = query_error; return -1; }
    va_list args;
    va_start(args, request);
    *va_arg(args, struct v4l2_capability *) = reply;
    va_end(args);
    return 0;
}
#define open fake_open
#define close fake_close
#define ioctl fake_ioctl
'''
checks = r'''
static void check(uint32_t all, uint32_t node, int expected, int planar) {
    video_data data = {0};
    AVFormatContext ctx = { .priv_data = &data };
    reply = (struct v4l2_capability) { .capabilities = all, .device_caps = node };
    opens = closes = 0;
    assert(device_open(&ctx, "/dev/video-fixture") == expected);
    assert(opens == 1 && closes == (expected < 0));
    if (expected >= 0) {
        assert(data.multiplanar == planar);
        assert(data.buf_type == (planar ? V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE
                                       : V4L2_BUF_TYPE_VIDEO_CAPTURE));
        data.close_f(expected);
        assert(closes == 1);
    }
}
int main(void) {
    uint32_t video = V4L2_CAP_VIDEO_CAPTURE | V4L2_CAP_STREAMING;
    uint32_t metadata = V4L2_CAP_META_CAPTURE | V4L2_CAP_STREAMING;
    uint32_t all = video | metadata | V4L2_CAP_DEVICE_CAPS;
    // Framework/UVC: same name and aggregate capabilities, different nodes.
    check(all, metadata, -ENODEV, 0);
    check(all, video, 17, 0);
    // Legacy drivers have no device_caps flag; keep their aggregate behavior.
    check(video, 0, 17, 0);
    check(metadata, 0, -ENODEV, 0);
    // Use the node's buffer type even if the physical device supports both.
    check(all | V4L2_CAP_VIDEO_CAPTURE_MPLANE,
          V4L2_CAP_VIDEO_CAPTURE_MPLANE | V4L2_CAP_STREAMING, 17, 1);
    // Missing streaming support cannot be borrowed from a sibling node.
    check(all, V4L2_CAP_VIDEO_CAPTURE, -ENOSYS, 0);
    check(all, 0, -ENODEV, 0);
    query_error = EACCES;
    check(all, video, -EACCES, 0);
    return 0;
}
'''
with tempfile.TemporaryDirectory(prefix='uxduck-v4l2-test-') as tmp:
    path = Path(tmp) / 'device-open.c'
    path.write_text(harness + setup + checks)
    binary = Path(tmp) / 'device-open'
    subprocess.run(['cc', '-std=c11', '-D_DEFAULT_SOURCE', '-Wall', '-Werror',
                    str(path), '-o', str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
print('V4L2 node capabilities: 8 cases passed')
