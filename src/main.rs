use anyhow::{Result as AnyResult, bail};
use bytes::Bytes;
use image::ImageReader;
use image::imageops::FilterType;
use image::{GenericImageView, ImageFormat};
use log::{error, info, warn};
use std::io;
use std::io::Cursor;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{File as TgFile, FileId, InputFile, ReplyParameters, StickerFormat};
use tempfile::{NamedTempFile, TempPath};
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

const FFMPEG_ARGS_WEBM_TO_GIF: (&[&str], &[&str]) =
    (&["-hide_banner", "-i"], &["-c:v", "gif", "-f", "gif", "-"]);

const TGS_TO_GIF: &str = "tgs_to_gif.sh";

#[derive(Debug, Clone)]
struct Blob {
    data: Bytes,
    ext: &'static str,
}

impl Blob {
    pub fn new<T: Into<Bytes>>(data: T, ext: &'static str) -> Self {
        Self {
            data: data.into(),
            ext,
        }
    }

    pub fn into_input_file(self, base: Option<&str>) -> InputFile {
        let n = self.data.len();
        let f = InputFile::memory(self.data);
        let mut out_name;
        if let Some(s) = base {
            out_name = s.to_owned();
            out_name.push('.');
        } else {
            out_name = "out.".to_owned();
        }
        out_name.push_str(self.ext);
        info!("sending {n} B as {out_name}");
        f.file_name(out_name)
    }
}

async fn wait_output(cmd: &mut Command) -> io::Result<Output> {
    let ch = cmd.kill_on_drop(true).spawn()?;
    match tokio::time::timeout(Duration::from_secs(60), ch.wait_with_output()).await {
        Ok(r) => r,
        Err(_) => {
            // kill_on_drop takes effect hopefully.
            Err(io::Error::new(io::ErrorKind::TimedOut, "child timed out"))
        }
    }
}

async fn temp_file() -> io::Result<(TempPath, File)> {
    let path = NamedTempFile::new()?.into_temp_path();
    let f = File::create(&path).await?;
    Ok((path, f))
}

fn process_image(file: Vec<u8>) -> AnyResult<Blob> {
    match ImageReader::new(Cursor::new(file))
        .with_guessed_format()
        .unwrap()
        .decode()
    {
        Ok(img) => {
            info!("got img of {:?}", img.dimensions());
            let img = img.resize(512, 512, FilterType::Lanczos3);
            // webp::Encoder sometimes fails with Unimplemented when inputting small images.
            Ok(match WebpEncoder::from_image(&img) {
                Ok(webp) => {
                    let mem = webp.encode_lossless();
                    Blob::new(mem.to_vec(), "webp")
                }
                Err(e) => {
                    warn!("webp: {e}, falling back to png");
                    let mut v = Cursor::new(Vec::with_capacity(60000));
                    img.write_to(&mut v, ImageFormat::Png)?;
                    Blob::new(v.into_inner(), "png")
                }
            })
        }
        Err(e) => {
            info!("decode failed: {e}");
            bail!("File is not an image.")
        }
    }
}

// Passing a mp4 video from pipe sometimes causes failure in codecs detection of ffmpeg, so we have
// to use a temporary file.
async fn process_video(file: &Path) -> AnyResult<Blob> {
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
        let out = wait_output(cmd.args(FFMPEG_ARGS.1).stdout(Stdio::piped())).await?;

        if !out.status.success() {
            error!("ffmpeg failed: {:?}", out.status);
            bail!("ffmpeg")
        }
        if !lossy && out.stdout.len() > MAX_OUTPUT_WEBM_SIZE {
            lossy = true;
            info!("retrying with lossy");
        } else {
            return Ok(Blob::new(out.stdout, "webm"));
        }
    }
}

async fn ffmpeg_to_gif(data: &[u8]) -> AnyResult<Blob> {
    // Using a pipe for ffmpeg stdin sometimes causes deadlock here.
    let (path, mut tmp) = temp_file().await?;
    tmp.write_all(data).await?;
    drop(tmp);

    let out = wait_output(
        Command::new(FFMPEG)
            .args(FFMPEG_ARGS_WEBM_TO_GIF.0)
            .arg(&path)
            .args(FFMPEG_ARGS_WEBM_TO_GIF.1)
            .stdout(Stdio::piped()),
    )
    .await?;
    if !out.status.success() {
        error!("ffmpeg failed: {:?}", out.status);
        bail!("ffmpeg")
    }
    Ok(Blob::new(out.stdout, "gif"))
}

async fn tgs_to_gif(file: &Path) -> AnyResult<Blob> {
    let out = wait_output(
        Command::new(TGS_TO_GIF)
            .arg(file)
            .args(["--output", "-"])
            .stdout(Stdio::piped()),
    )
    .await?;
    if !out.status.success() {
        error!("tgs_to_gif failed: {:?}", out.status);
        bail!("tgs_to_gif")
    }
    Ok(Blob::new(out.stdout, "gif"))
}

#[derive(Debug, Clone)]
struct Request<'a> {
    msg: Message,
    bot: Bot,
    caption: Option<&'a str>,
    base: Option<&'a str>,
}

#[derive(Debug, Clone)]
enum Op {
    Image,
    Video,
    Sticker(StickerFormat),
}

impl Request<'_> {
    async fn download_mem(&self, f: TgFile) -> AnyResult<Vec<u8>> {
        let mut v = Vec::with_capacity(f.size as usize);
        self.bot.download_file(&f.path, &mut v).await?;
        info!("download_mem: {} B", v.len());
        Ok(v)
    }

    async fn download_tmp(&self, f: TgFile) -> AnyResult<TempPath> {
        let (path, mut tmp) = temp_file().await?;
        self.bot.download_file(&f.path, &mut tmp).await?;
        drop(tmp);
        info!("download_tmp: {} B", f.size);
        Ok(path)
    }

    async fn handle_image(&self, f: TgFile) -> AnyResult<Blob> {
        let v = self.download_mem(f).await?;
        process_image(v)
    }

    async fn handle_video(&self, f: TgFile) -> AnyResult<Blob> {
        let path = self.download_tmp(f).await?;
        process_video(&path).await
    }

    async fn handle_sticker(&self, f: TgFile, fmt: StickerFormat) -> AnyResult<()> {
        match fmt {
            StickerFormat::Static => {
                self.send_raw(Blob::new(self.download_mem(f).await?, "webp"))
                    .await
            }
            StickerFormat::Animated => {
                let path = self.download_tmp(f).await?;
                self.send_raw(tgs_to_gif(&path).await?).await
            }
            StickerFormat::Video => {
                let data = bytes::Bytes::from(self.download_mem(f).await?);
                let (r1, r2) = join!(self.send_raw(Blob::new(data.clone(), "webm")), async move {
                    self.send_raw(ffmpeg_to_gif(&data).await?).await
                });
                r1?;
                r2
            }
        }
    }

    async fn handle_media(&self, file_id: FileId, op: Op) -> AnyResult<()> {
        let f = self.bot.get_file(file_id).await?;
        if f.size > MAX_SIZE {
            bail!("File too big")
        }
        match op {
            Op::Image => self.send(self.handle_image(f).await?).await,
            Op::Video => self.send(self.handle_video(f).await?).await,
            Op::Sticker(fmt) => self.handle_sticker(f, fmt).await,
        }
    }

    async fn send_raw(&self, b: Blob) -> AnyResult<()> {
        let f = self.get_input_file(b);
        let mut p = self
            .bot
            .send_document(self.msg.chat.id, f)
            .reply_parameters(ReplyParameters::new(self.msg.id).allow_sending_without_reply());
        if let Some(s) = self.caption {
            p.caption = Some(s.to_string());
        }
        p.disable_content_type_detection = Some(true);
        if let Err(e) = p.await {
            error!("send_document: {e}");
            bail!("Failed to send file.")
        }
        Ok(())
    }

    async fn send(&self, b: Blob) -> AnyResult<()> {
        let f = self.get_input_file(b);
        let mut p = self
            .bot
            .send_document(self.msg.chat.id, f)
            .reply_parameters(ReplyParameters::new(self.msg.id).allow_sending_without_reply());
        if let Some(s) = self.caption {
            p.caption = Some(s.to_string());
        }
        if let Err(e) = p.await {
            error!("send_document: {e}");
            bail!("Failed to send file.")
        }
        Ok(())
    }

    fn get_input_file(&self, blob: Blob) -> InputFile {
        blob.into_input_file(self.base)
    }

    async fn handler(mut self) -> &'static str {
        let ch = &self.msg.chat;
        info!(
            "from {} {} (@{} {})",
            ch.first_name().unwrap_or(""),
            ch.last_name().unwrap_or(""),
            ch.username().unwrap_or(""),
            ch.id.0
        );
        let msg = &self.msg;
        let mut op = Op::Image;
        let (file_id, size, file_name) = if let Some(doc) = msg.document() {
            info!(
                "got document {} of {} bytes",
                doc.file_name.as_deref().unwrap_or(""),
                doc.file.size
            );
            if let Some(s) = &doc.file_name
                && Path::new(s)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("gif"))
            {
                op = Op::Video;
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
                "got {:?} sticker in {} {} of {} x {}, {} B",
                sti.format(),
                sti.set_name.as_deref().unwrap_or(""),
                sti.emoji.as_deref().unwrap_or(""),
                sti.width,
                sti.height,
                sti.file.size
            );
            op = Op::Sticker(sti.format());
            self.caption = sti.emoji.as_ref().map(std::convert::AsRef::as_ref);
            (&sti.file.id, sti.file.size, sti.set_name.as_ref())
        } else if Some("/start") == msg.text() {
            return "Hi! Send me an image or a GIF, and I'll convert it for use with @Stickers. Also, I can convert stickers to images or GIFs.";
        } else {
            info!("invalid: {msg:#?}");
            return "Please send an image, a GIF, or a sticker.";
        };
        if size > MAX_SIZE {
            return "File is too big.";
        }
        self.base = file_name.map(std::convert::AsRef::as_ref);
        if let Err(e) = self.handle_media(file_id.clone(), op).await {
            error!("handle: {e:?}");
            return e
                .downcast::<&'static str>()
                .unwrap_or("Something went wrong.");
        }
        ""
    }
}

#[tokio::main]
async fn main() {
    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var("RUST_LOG", "info");
        }
    }
    pretty_env_logger::init();

    let bot = Bot::from_env();
    info!("bot started: {:?}", bot.client());

    teloxide::repl(bot, |msg: Message, bot: Bot| async move {
        tokio::spawn(async move {
            let id = msg.chat.id;
            let req = Request {
                msg,
                bot: bot.clone(),
                caption: None,
                base: None,
            };
            let s = req.handler().await;
            if !s.is_empty()
                && let Err(e) = bot.send_message(id, s).await
            {
                error!("send_message: {e:?}");
            }
        });
        // TODO: join the spawned tasks when interrupted?
        Ok(())
    })
    .await;
}
