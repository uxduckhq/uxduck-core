"""Exercise the patched AVFoundation stream setup with synthetic CoreMedia frames.

Run on macOS after building FFmpeg. No physical camera or camera permission is
needed. FFmpeg allocation/locking is stubbed; the stream-setup function and
CoreMedia sample timing are the real implementations.
"""
from pathlib import Path
import subprocess
import sys
import tempfile

ffmpeg = Path(sys.argv[1]).resolve()
source = (ffmpeg / 'libavdevice/avfoundation.m').read_text()
start = source.index('static int get_video_config(')
end = source.index('\nstatic int get_audio_config(', start)
setup = source[start:end]
harness = r'''
#import <AVFoundation/AVFoundation.h>
#include <assert.h>
#include <limits.h>
#include <string.h>
#include "libavutil/avutil.h"
#include "libavutil/rational.h"
#include "libavcodec/codec_id.h"

// Only allocation and lock bookkeeping are replaced. The function under test
// below is copied verbatim from the source compiled into the capture helper.
typedef struct { int codec_id, codec_type, width, height, format; } AVCodecParameters;
typedef struct {
    int index;
    AVCodecParameters *codecpar;
    AVRational avg_frame_rate, r_frame_rate;
} AVStream;
typedef struct { void *priv_data; } AVFormatContext;
typedef struct {
    int frames_captured, video_stream_index, video_is_screen, pixel_format, locks;
    CMSampleBufferRef current_frame;
    AVRational framerate;
} AVFContext;
static AVStream stream;
static AVCodecParameters parameters;
static const int avf_time_base = 1000000;
static AVStream *avformat_new_stream(AVFormatContext *s, const void *codec) {
    memset(&stream, 0, sizeof(stream));
    memset(&parameters, 0, sizeof(parameters));
    stream.codecpar = &parameters;
    return &stream;
}
static void avpriv_set_pts_info(AVStream *s, int bits, unsigned num, unsigned den) {
    assert(num == 1 && den == 1000000);
}
static void lock_frames(AVFContext *ctx) { ++ctx->locks; }
static void unlock_frames(AVFContext *ctx) { assert(ctx->locks == 1); --ctx->locks; }
#define AV_LOG_ERROR 16
#define av_log(...) ((void)0)

@interface TestCamera : NSObject
@property CMTime activeVideoMinFrameDuration;
@end
@implementation TestCamera
@end

/* STREAM_SETUP */

static void check(CMTime duration, CMTime active_duration, AVRational requested,
                  AVRational expected, int expected_result) {
    CVPixelBufferRef image = NULL;
    CMVideoFormatDescriptionRef description = NULL;
    CMSampleBufferRef sample = NULL;
    assert(CVPixelBufferCreate(kCFAllocatorDefault, 64, 48,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, NULL, &image) == kCVReturnSuccess);
    assert(CMVideoFormatDescriptionCreateForImageBuffer(kCFAllocatorDefault,
        image, &description) == noErr);
    CMSampleTimingInfo timing = {duration, CMTimeMake(100, 1), kCMTimeInvalid};
    assert(CMSampleBufferCreateForImageBuffer(kCFAllocatorDefault, image, true,
        NULL, NULL, description, &timing, &sample) == noErr);
    AVFContext ctx = {.frames_captured = 1, .current_frame = sample, .framerate = requested};
    AVFormatContext format = {.priv_data = &ctx};
    TestCamera *camera = [[TestCamera alloc] init];
    camera.activeVideoMinFrameDuration = active_duration;
    assert(get_video_config(&format, (AVCaptureDevice *)camera) == expected_result);
    assert(ctx.locks == 0);
    if (expected_result == 0) {
        assert(av_cmp_q(stream.avg_frame_rate, expected) == 0);
        assert(av_cmp_q(stream.r_frame_rate, expected) == 0);
        assert(parameters.width == 64 && parameters.height == 48);
        assert(ctx.current_frame == NULL);
    }
    if (ctx.current_frame) CFRelease(ctx.current_frame);
    [camera release];
    CFRelease(description);
    CFRelease(image);
}
int main(void) {
    @autoreleasepool {
        AVRational automatic = {0, 1};
        check(CMTimeMake(1, 30), kCMTimeInvalid, automatic, (AVRational){30, 1}, 0);
        check(CMTimeMake(1, 25), kCMTimeInvalid, automatic, (AVRational){25, 1}, 0);
        check(CMTimeMake(1001, 30000), kCMTimeInvalid, automatic, (AVRational){30000, 1001}, 0);
        check(kCMTimeInvalid, CMTimeMake(1, 30), automatic, (AVRational){30, 1}, 0);
        check(kCMTimeInvalid, kCMTimeInvalid, (AVRational){25, 1}, (AVRational){25, 1}, 0);
        check(kCMTimeInvalid, kCMTimeInvalid, automatic, automatic, 1);
    }
    return 0;
}
'''.replace('/* STREAM_SETUP */', setup)
with tempfile.TemporaryDirectory(prefix='uxduck-frame-rate-') as directory:
    path = Path(directory)
    (path / 'test.m').write_text(harness)
    subprocess.run([
        'clang', '-I', str(ffmpeg), str(path / 'test.m'),
        str(ffmpeg / 'libavutil/libavutil.a'),
        '-framework', 'AVFoundation', '-framework', 'Foundation',
        '-framework', 'CoreMedia', '-framework', 'CoreVideo',
        '-o', str(path / 'test'),
    ], check=True)
    subprocess.run([str(path / 'test')], check=True)
print('AVFoundation frame timing: 6 synthetic stream-setup checks passed')
