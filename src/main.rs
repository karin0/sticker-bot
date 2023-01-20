use anyhow::{bail, Result as AnyResult};
use bytes::Bytes;
use image::imageops::FilterType;
use image::io::Reader as ImageReader;
use image::{GenericImageView, ImageOutputFormat};
use log::{error, info, warn};
use std::io::Cursor;
use std::path::Path;
use std::process::Stdio;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{File as TgFile, InputFile, StickerFormat};
use tempfile::NamedTempFile;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::join;
use tokio::process::Command;
use webp::Encoder as WebpEncoder;

const MAX_SIZE: u32 = 10 << 20;
const MAX_OUTPUT_WEBM_SIZE: usize = 256 * 1000;

const FFMPEG: &str = "ffmpeg";

const FFMPEG_ARGS: (&[&str], &[&str]) = (
    &["-hide_banner", "-t", "3", "-i"],
    &[
        "-vf",
        "scale=w=512:h=512:force_original_aspect_ratio=decrease",
        "-c:v",
        "libvpx-vp9",
        "-f",
        "webm",
        "-an",
        "-",
    ],
);

async fn process_image(file: Vec<u8>) -> AnyResult<(Bytes, &'static str)> {
    match ImageReader::new(Cursor::new(file))
        .with_guessed_format()
        .unwrap()
        .decode()
    {
        Ok(img) => {
            info!("got img of {:?}", img.dimensions());
            let img = img.resize(512, 512, FilterType::Lanczos3);
            // webp::Encoder sometimes fails with Unimplemented when inputting small images.
            return Ok(match WebpEncoder::from_image(&img) {
                Ok(webp) => {
                    let mem = webp.encode_lossless();
                    (Bytes::copy_from_slice(&mem), "webp")
                }
                Err(e) => {
                    warn!("webp: {}, falling back to png", e);
                    let mut v = Cursor::new(Vec::with_capacity(60000));
                    img.write_to(&mut v, ImageOutputFormat::Png)?;
                    (v.into_inner().into(), "png")
                }
            });
        }
        Err(e) => {
            info!("decode failed: {}", e);
            bail!("File is not an image.")
        }
    }
}

// Passing a mp4 video from pipe sometimes causes failure in codecs detection of ffmpeg, so we have
// to use a temporary file.
async fn process_video(file: &Path) -> AnyResult<(Bytes, &'static str)> {
    // FIXME: output could be still too big even when lossy, try specify a bit rate?
    // FIXME: current implementation often has to run ffmpeg twice, try to avoid the lossless
    //        attempt in such cases.

    let mut lossy = false;
    loop {
        let mut cmd = Command::new(FFMPEG);
        let mut cmd = cmd.args(FFMPEG_ARGS.0).arg(file);
        if !lossy {
            cmd = cmd.arg("-lossless").arg("1");
        }
        let out = cmd
            .args(FFMPEG_ARGS.1)
            .stdout(Stdio::piped())
            .spawn()?
            .wait_with_output()
            .await?;

        if !out.status.success() {
            error!("ffmpeg failed: {:?}", out.status);
            bail!("ffmpeg")
        }
        if !lossy && out.stdout.len() > MAX_OUTPUT_WEBM_SIZE {
            lossy = true;
            info!("retrying with lossy");
        } else {
            return Ok((Bytes::from(out.stdout), "webm"));
        }
    }
}

async fn handle_image(f: TgFile, bot: &Bot) -> AnyResult<(Bytes, &'static str)> {
    let mut v = Vec::with_capacity(f.size as usize);
    bot.download_file(&f.path, &mut v).await?;
    info!("downloaded {} bytes", v.len());
    process_image(v).await
}

async fn handle_video(f: TgFile, bot: &Bot) -> AnyResult<(Bytes, &'static str)> {
    let path = NamedTempFile::new()?.into_temp_path();
    let mut tmp = File::create(&path).await?;
    bot.download_file(&f.path, &mut tmp).await?;
    tmp.flush().await?;
    drop(tmp);
    info!("downloaded {} bytes", f.size);
    process_video(&path).await
}

#[derive(Debug, Clone)]
enum Op {
    Image,
    Video,
    Sticker(StickerFormat),
}

#[derive(Copy, Debug, Clone)]
enum Action {
    Raw,
    Photo,
}

async fn handle_media(
    file_id: &String,
    bot: &Bot,
    op: Op,
) -> AnyResult<(Bytes, &'static str, Action)> {
    let f = bot.get_file(file_id).await?;
    if f.size > MAX_SIZE {
        bail!("File too big")
    }
    match op {
        Op::Image => handle_image(f, bot).await.map(|(a, b)| (a, b, Action::Raw)),
        Op::Video => handle_video(f, bot).await.map(|(a, b)| (a, b, Action::Raw)),
        Op::Sticker(fmt) => {
            let mut v = Vec::with_capacity(f.size as usize);
            bot.download_file(&f.path, &mut v).await?;
            info!("downloaded {} bytes", v.len());
            let mut action = Action::Raw;
            let suffix = match fmt {
                // Name the webp file as png to avoid sending the sticker again.
                StickerFormat::Raster => {
                    action = Action::Photo;
                    "png"
                }
                StickerFormat::Video => "webm",
                // TODO: Convert tgs to a portable format
                StickerFormat::Animated => "tgs",
            };
            Ok((Bytes::from(v), suffix, action))
        }
    }
}

async fn reply_photo(
    msg: &Message,
    bot: &Bot,
    f: InputFile,
    caption: Option<&str>,
) -> AnyResult<()> {
    let mut p = bot.send_photo(msg.chat.id, f);
    if let Some(s) = caption {
        p.caption = Some(s.to_string());
    }
    p.reply_to_message_id = Some(msg.id);
    if let Err(e) = p.await {
        error!("send_photo: {}", e);
        bail!("Failed to send photo.")
    } else {
        Ok(())
    }
}

async fn reply_document(
    msg: &Message,
    bot: &Bot,
    f: InputFile,
    caption: Option<&str>,
) -> AnyResult<()> {
    let mut p = bot.send_document(msg.chat.id, f);
    if let Some(s) = caption {
        p.caption = Some(s.to_string());
    }
    p.reply_to_message_id = Some(msg.id);
    if let Err(e) = p.await {
        error!("send_document: {}", e);
        bail!("Failed to send file.")
    } else {
        Ok(())
    }
}

async fn do_reply(
    msg: &Message,
    bot: &Bot,
    f: InputFile,
    action: Action,
    caption: Option<&str>,
) -> AnyResult<&'static str> {
    match action {
        Action::Raw => {
            reply_document(msg, bot, f, caption).await?;
        }
        Action::Photo => {
            let fut = reply_document(msg, bot, f.clone(), caption);
            let (r1, r2) = join!(fut, reply_photo(msg, bot, f, caption));
            r1?;
            r2?;
        }
    }
    Ok("")
}

fn extract_error(e: anyhow::Error) -> &'static str {
    e.downcast::<&'static str>()
        .unwrap_or("Something went wrong.")
}

async fn handler(msg: Message, bot: &Bot) -> &'static str {
    let ch = &msg.chat;
    info!(
        "from {} {} (@{} {})",
        ch.first_name().unwrap_or(""),
        ch.last_name().unwrap_or(""),
        ch.username().unwrap_or(""),
        ch.id.0
    );
    let mut op = Op::Image;
    let mut caption = None;
    let (file_id, size, file_name) = if let Some(doc) = msg.document() {
        info!(
            "got document {} of {} bytes",
            doc.file_name.as_deref().unwrap_or(""),
            doc.file.size
        );
        if let Some(s) = &doc.file_name {
            if s.ends_with(".gif") {
                op = Op::Video;
            }
        }
        (&doc.file.id, doc.file.size, doc.file_name.as_ref())
    } else if let Some(sizes) = msg.photo() {
        let ph = sizes
            .iter()
            .find(|ph| ph.width >= 512 || ph.height >= 512)
            .unwrap_or_else(|| sizes.last().unwrap());
        info!(
            "got photo of {} x {}, {} B",
            ph.width, ph.height, ph.file.size
        );
        (&ph.file.id, ph.file.size, None)
    } else if let Some(ani) = msg.animation() {
        info!(
            "got animation {} of {} x {}, {} s, {} B",
            ani.file_name.as_deref().unwrap_or(""),
            ani.width,
            ani.height,
            ani.duration,
            ani.file.size
        );
        op = Op::Video;
        (&ani.file.id, ani.file.size, ani.file_name.as_ref())
    } else if let Some(sti) = msg.sticker() {
        info!(
            "got sticker in {} of {} x {}, {} B",
            sti.set_name.as_deref().unwrap_or(""),
            sti.width,
            sti.height,
            sti.file.size
        );
        op = Op::Sticker(sti.format.clone());
        caption = sti.emoji.as_ref();
        (&sti.file.id, sti.file.size, sti.set_name.as_ref())
    } else if Some("/start") == msg.text() {
        return "Hi! Send me an image or a GIF animation, and I'll convert it for use with @Stickers.";
    } else {
        info!("invalid: {:#?}", msg);
        return "Please send an image or an GIF animation.";
    };

    if size > MAX_SIZE {
        return "File is too big.";
    }

    match handle_media(file_id, bot, op).await {
        Ok((f, suf, action)) => {
            let n = f.len();
            let f = InputFile::memory(f);
            let mut out_name;
            if let Some(s) = file_name {
                out_name = s.clone();
                out_name.push('.');
            } else {
                out_name = "out.".to_owned();
            };
            out_name.push_str(suf);
            info!("sending {}, {} bytes", out_name, n);
            let f = f.file_name(out_name);

            let caption: Option<&str> = match caption {
                Some(s) => Some(s),
                None => None,
            };
            if let Err(e) = do_reply(&msg, bot, f, action, caption).await {
                return extract_error(e);
            }
        }
        Err(e) => {
            error!("handle: {:?}", e);
            return extract_error(e);
        }
    }
    ""
}

#[tokio::main]
async fn main() {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    pretty_env_logger::init();

    let bot = Bot::from_env();
    info!("bot started: {:?}", bot.client());

    teloxide::repl(bot, |msg: Message, bot: Bot| async move {
        tokio::spawn(async move {
            let id = msg.chat.id;
            let s = handler(msg, &bot).await;
            if !s.is_empty() {
                if let Err(e) = bot.send_message(id, s).await {
                    error!("send_message: {:?}", e);
                }
            }
        });
        // TODO: join the spawned tasks when interrupted?
        Ok(())
    })
    .await;
}
