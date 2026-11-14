# Example clips

Media for `examples/showcase.typ`:

- `big-lebowski-1.gif`: animated GIF, 350 x 480.
- `spl.mp4`: H.264 High, 1920 x 1080, 60 fps, 27 s (its AAC sound is not played).
- `spl.svg`: the poster shown in place of `spl.mp4`.
- `buck-bunny.jpg`: the poster of the web video.

The web examples use the GIF file behind https://giphy.com/gifs/big-lebowski-yk8tRCZHCV0qY (`https://media.giphy.com/media/yk8tRCZHCV0qY/giphy.gif`) and a direct MP4 (Big Buck Bunny on test-videos.co.uk). YouTube pages serve no video file and cannot be used.

Rebuild the deck from the repository root:

```bash
typst compile --root . examples/showcase.typ examples/showcase.pdf
```

To embed a screen recording (for example a macOS `.mov`), convert it to H.264 MP4 without sound first:

```bash
ffmpeg -i recording.mov -c:v libx264 -preset slow -crf 26 -pix_fmt yuv420p -vf "scale=1920:-2" -an -movflags +faststart examples/clips/recording.mp4
```

In `showcase.typ`, pass it to a screenshot slot: `shot("/img/screenshot-drawing.png", width: 10.6cm, ratio: 3549 / 2024, video: "examples/clips/recording.mp4")`. The recording then plays over the screenshot.
