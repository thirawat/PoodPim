Test files for `audio::load_audio`: one second of a 440 Hz tone each, made with ffmpeg 8.

```sh
f() { ffmpeg -hide_banner -loglevel error -y "$@"; }
f -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=1" -ac 2 -c:a aac -b:a 64k tone.m4a
f -f lavfi -i "sine=frequency=440:sample_rate=22050:duration=1" -c:a libmp3lame -b:a 32k tone.mp3
f -f lavfi -i "testsrc=size=64x64:rate=5:duration=1" -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=1" \
  -map 0:v -map 1:a -c:v libx264 -pix_fmt yuv420p -c:a aac -b:a 48k tone.mp4
f -f lavfi -i "testsrc=size=64x64:rate=5:duration=1" -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=1" \
  -map 0:v -map 1:a -c:v libx264 -pix_fmt yuv420p -c:a aac -b:a 48k tone.mov
f -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=1" -c:a alac tone-alac.m4a
```
