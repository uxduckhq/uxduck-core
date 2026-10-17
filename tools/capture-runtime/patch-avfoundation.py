"""Add exact device identity to the pinned FFmpeg AVFoundation input.

FFmpeg's name lookup is a prefix match; never let two USB cameras exchange
identities after a reconnect. This patch is shipped with corresponding source.
"""
from pathlib import Path
import sys
p = Path(sys.argv[1]) / 'libavdevice/avfoundation.m'
s = p.read_text()
def replace(old, new, count=1):
    global s
    if s.count(old) != count:
        raise SystemExit(f'FFmpeg source changed: expected {count} occurrences of {old!r}')
    s = s.replace(old, new)
replace('    char            *video_filename;', '    char            *video_filename;\n    char            *video_device_id;')
replace('av_log(ctx, AV_LOG_INFO, "[%d] %s\\n", index, name);', 'av_log(ctx, AV_LOG_INFO, "[%d] %s  [uid:%s]\\n", index, name, [[device uniqueID] UTF8String]);', 3)
replace('    if (ctx->video_device_index >= 0) {', '''    if (ctx->video_device_id) {
        for (AVCaptureDevice *device in devices) {
            if (!strcmp(ctx->video_device_id, [[device uniqueID] UTF8String])) {
                video_device = device;
                break;
            }
        }
        for (AVCaptureDevice *device in devices_muxed) {
            if (!strcmp(ctx->video_device_id, [[device uniqueID] UTF8String])) {
                video_device = device;
                ctx->video_is_muxed = 1;
                break;
            }
        }
        if (!video_device) {
            av_log(ctx, AV_LOG_ERROR, "The selected camera is not connected\\n");
            goto fail;
        }
    } else if (ctx->video_device_index >= 0) {''')
needle='static const AVOption options[] = {'
replace(needle, needle+'\n    { "video_device_id", "select the exact AVFoundation unique ID", offsetof(AVFContext, video_device_id), AV_OPT_TYPE_STRING, { .str = NULL }, 0, 0, AV_OPT_FLAG_DECODING_PARAM },')
# A zero frame rate means preserve the camera's active mode. FFmpeg's
# default 29.97fps rejects the many webcams that support exactly 30fps.
# VIDEO_RATE rejects zero before configure_video_device runs. RATIONAL keeps
# the same storage/default/range while allowing our zero sentinel through.
replace('offsetof(AVFContext, framerate), AV_OPT_TYPE_VIDEO_RATE, {.str = "ntsc"}',
        'offsetof(AVFContext, framerate), AV_OPT_TYPE_RATIONAL, {.dbl = 30000.0 / 1001}')
replace('    double framerate = av_q2d(ctx->framerate);', '''    double framerate = av_q2d(ctx->framerate);
    if (framerate == 0 && ctx->width == 0 && ctx->height == 0) {
        // Keep the active mode locked until startRunning, just like the
        // explicit-mode path. The session may otherwise reconfigure it.
        if ([video_device lockForConfiguration:NULL] == YES)
            return 0;
        av_log(s, AV_LOG_ERROR, "Could not lock device for configuration.\\n");
        return AVERROR(EINVAL);
    }''')
replace('if (fabs (framerate - max_framerate) < 0.01)', 'if (framerate == 0 || fabs (framerate - max_framerate) < 0.01)')
# Raw frames can exceed FFmpeg's probe-size budget before enough timestamps
# arrive to estimate a rate. Without declared rates it falls back to the 1 MHz
# timestamp clock, starving x264's per-picture bitrate budget.
replace('static int get_video_config(AVFormatContext *s)',
        'static int get_video_config(AVFormatContext *s, AVCaptureDevice *video_device)')
replace('get_video_config(s)', 'get_video_config(s, video_device)')
replace('    avpriv_set_pts_info(stream, 64, 1, avf_time_base);\n\n    image_buffer = CMSampleBufferGetImageBuffer(ctx->current_frame);', '''    avpriv_set_pts_info(stream, 64, 1, avf_time_base);

    CMTime frame_duration = CMSampleBufferGetDuration(ctx->current_frame);
    if ((!CMTIME_IS_NUMERIC(frame_duration) || frame_duration.value <= 0) && !ctx->video_is_screen)
        frame_duration = [video_device activeVideoMinFrameDuration];
    AVRational frame_rate = ctx->framerate;
    if (CMTIME_IS_NUMERIC(frame_duration) && frame_duration.value > 0)
        frame_rate = av_d2q(frame_duration.timescale / (double)frame_duration.value, INT_MAX);
    if (frame_rate.num <= 0 || frame_rate.den <= 0) {
        av_log(s, AV_LOG_ERROR, "The camera did not report a valid frame rate.\\n");
        unlock_frames(ctx);
        return 1;
    }
    stream->avg_frame_rate = stream->r_frame_rate = frame_rate;

    image_buffer = CMSampleBufferGetImageBuffer(ctx->current_frame);''')
p.write_text(s)
