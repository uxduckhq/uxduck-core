#!/usr/bin/env bash
# Native build, or either macOS architecture. Windows: MSYS2 MINGW64 shell.
# Run through node scripts/capture-runtime.ts for verified sources and caching.
set -euo pipefail
work=$1
runtime=$2
if command -v cygpath >/dev/null 2>&1; then
  work=$(cygpath -u "$work")
  runtime=$(cygpath -u "$runtime")
fi
arch=$3
prefix="$work/prefix"
mkdir -p "$prefix" "$runtime"
# Deliberately keep this small and independent of Homebrew codec libraries.
# x264 is a separately executed GPL helper, never linked into UXDuck.
x264_options=(--prefix="$prefix" --enable-static --disable-cli --disable-opencl)
ffmpeg_options=(--prefix="$prefix" --disable-shared --enable-static --disable-autodetect --disable-doc --disable-debug --disable-network --disable-programs --enable-ffmpeg --disable-everything --enable-gpl --enable-libx264 --enable-pthreads --enable-protocol=pipe,file --enable-demuxer=mov,mpegts --enable-decoder=aac --enable-parser=aac --enable-indev=lavfi --enable-decoder=wrapped_avframe,rawvideo,mjpeg,mjpegb,h264,hevc,dvvideo,huffyuv,mpeg2video,pcm_s16le,pcm_s16be,pcm_s24le,pcm_s24be,pcm_s32le,pcm_s32be,pcm_f32le,pcm_f32be,pcm_u8 --enable-parser=h264,hevc,mjpeg,mpegvideo --enable-encoder=libx264,aac --enable-muxer=mpegts --enable-filter=buffer,buffersink,scale,format,setsar,bwdif,testsrc2,abuffer,abuffersink,aformat,aresample,anull,sine --enable-swscale --enable-swresample --enable-avfilter --enable-avdevice)
# A camera's sound (S148): PCM from the input, encoded to AAC by FFmpeg's own
# encoder, resampled as needed.
# make's target is the program's file name, ffmpeg.exe on Windows.
program=ffmpeg
case "$(uname -s)" in
  Darwin)
    export MACOSX_DEPLOYMENT_TARGET=13.0
    macarch=$arch
    [ "$macarch" != aarch64 ] || macarch=arm64
    x264_options+=(--host="$arch-apple-darwin" --extra-cflags="-arch $macarch -mmacosx-version-min=13.0" --extra-ldflags="-arch $macarch -mmacosx-version-min=13.0")
    if [ "$macarch" != "$(uname -m)" ]; then
      ffmpeg_options+=(--enable-cross-compile --target-os=darwin)
    fi
    export CC="clang -arch $macarch"
    ffmpeg_options+=(--arch="$arch" --cc=clang --extra-cflags="-arch $macarch -mmacosx-version-min=13.0" --extra-ldflags="-arch $macarch -mmacosx-version-min=13.0" --enable-avfoundation --enable-indev=avfoundation --enable-videotoolbox --enable-encoder=h264_videotoolbox)
    ;;
  # Windows threads natively; pthreads there would need winpthreads, which
  # --disable-autodetect leaves out.
  MINGW*|MSYS*)
    ffmpeg_options=("${ffmpeg_options[@]/--enable-pthreads/--enable-w32threads}")
    ffmpeg_options+=(--enable-indev=dshow --target-os=mingw32 --arch=x86_64 --extra-ldflags=-static)
    program=ffmpeg.exe
    ;;
  # ALSA for a camera's sound input needs libasound's headers to build.
  Linux) ffmpeg_options+=(--enable-indev=v4l2 --enable-demuxer=video4linux2 --enable-alsa --enable-indev=alsa) ;;
  *) echo 'Unsupported capture runtime build host' >&2; exit 1 ;;
esac
(cd "$work/x264" && ./configure "${x264_options[@]}" && make -j2 && make install)
export PKG_CONFIG_PATH="$prefix/lib/pkgconfig"
(cd "$work/ffmpeg" && ./configure "${ffmpeg_options[@]}" && make -j2 "$program")
if [ -f "$work/ffmpeg/ffmpeg.exe" ]; then
  cp "$work/ffmpeg/ffmpeg.exe" "$runtime/ffmpeg.exe"
else
  cp "$work/ffmpeg/ffmpeg" "$runtime/ffmpeg"
fi
cp "$work/ffmpeg/COPYING.GPLv2" "$runtime/FFmpeg-LICENSE"
cp "$work/x264/COPYING" "$runtime/x264-LICENSE"
