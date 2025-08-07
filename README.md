# sticker-bot

A trivial Telegram bot that converts images/GIFs to stickers (for use in [@Stickers](http://t.me/Stickers)), and vice versa.

Demo: [@sticker_chan_bot](http://t.me/sticker_chan_bot)

## Features

- Image -> WebP sticker
- GIF -> WebM video sticker
- Sticker -> WebP/WebM/GIF file

## Running

Prepare the following dependencies in PATH:

- `ffmpeg`
- `gifski`
- `lottie_to_png` (Binary from [lottie-converter](https://github.com/ed-asriyan/lottie-converter/releases))
- [`lottie_to_gif.sh`](/lottie_to_gif.sh) (A patched one in this [repository](/lottie_to_gif.sh))

Install and run the bot using Cargo:

```console
$ cargo install --git https://github.com/karin0/sticker-bot
$ TELOXIDE_TOKEN=<your_bot_token> ~/.cargo/bin/sticker-bot
```

Windows is not supported for now.

## Credits

- [lottie-converter](https://github.com/ed-asriyan/lottie-converter)

## License

[MIT License](/LICENSE)
