# sticker-bot

A trivial Telegram bot that converts images/GIFs to stickers (for use in [@Stickers](http://t.me/Stickers)), and vice versa.

Demo: [@sticker_chan_bot](http://t.me/sticker_chan_bot)

## Features

- Image -> WebP sticker
- GIF -> WebM video sticker
- Sticker -> WebP/WebM/GIF file

## Running

Static image conversions require no external dependencies.

Video/animated sticker conversions involving GIFs are unavailable on Windows yet, and require the following dependencies in `PATH`:

- `ffmpeg` (WebM video sticker from/to GIF)
- `gifski` (TGS animated sticker to GIF)
- `lottie_to_png` (TGS to GIF)
  - Get the binary from [lottie-converter](https://github.com/ed-asriyan/lottie-converter/releases).
- [`lottie_to_gif.sh`](/lottie_to_gif.sh) (TGS to GIF)
  - Get the patched script [included](/lottie_to_gif.sh) in the repository.

Install and run the bot using Cargo:

```console
$ cargo install --git https://github.com/karin0/sticker-bot
$ TELOXIDE_TOKEN=<your_bot_token> ~/.cargo/bin/sticker-bot
```

## Credits

- [lottie-converter](https://github.com/ed-asriyan/lottie-converter)

## License

[MIT License](/LICENSE)
