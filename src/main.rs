use anyhow::{Result as AnyResult, bail};
use bytes::Bytes;
use image::ImageReader;
use image::imageops::FilterType;
use image::{GenericImageView, ImageFormat};
use log::{debug, error, info, warn};
use std::cell::RefCell;
use std::io;
use std::io::Cursor;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;
use teloxide::errors::RequestError;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{
    File as TgFile, FileId, InputFile, MessageId, ReplyParameters, StickerFormat,
};
use tempfile::{NamedTempFile, TempPath};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::join;
use tokio::process::Command;
use tokio::task::spawn;
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

const TGS_TO_GIF: &str = "lottie_to_gif.sh";

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

fn check_command(bin: &str, arg: &str) -> io::Result<()> {
    match std::process::Command::new(bin)
        .arg(arg)
        .stdout(Stdio::null())
        .spawn()
        .and_then(|mut c| c.wait())
    {
        Ok(r) => {
            if !r.success() {
                warn!("{bin} {arg} failed: {r}");
            }
            Ok(())
        }
        Err(e) => {
            error!("{bin} {arg}: {e}");
            Err(e)
        }
    }
}

#[derive(Debug, Clone)]
struct Request<'a, T: Fn(MessageId)> {
    msg: Message,
    bot: Bot,
    caption: Option<&'a str>,
    base: Option<&'a str>,
    msg_callback: T,
}

// Safety: single-threaded runtime :)
unsafe impl<T: Fn(MessageId)> Send for Request<'_, T> {}
unsafe impl<T: Fn(MessageId)> Sync for Request<'_, T> {}

#[derive(Debug, Clone)]
enum Op {
    Image,
    Video,
    Sticker(StickerFormat),
}

impl<T: Fn(MessageId)> Request<'_, T> {
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

    fn finalize_send(&self, r: Result<Message, RequestError>) -> AnyResult<()> {
        match r {
            Ok(msg) => {
                (self.msg_callback)(msg.id);
                Ok(())
            }
            Err(e) => {
                error!("send: {e}");
                bail!("Failed to send.")
            }
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
        self.finalize_send(p.await)
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
        self.finalize_send(p.await)
    }

    fn get_input_file(&self, blob: Blob) -> InputFile {
        blob.into_input_file(self.base)
    }

    async fn handler(mut self, user_id: UserId) -> &'static str {
        let chat = &self.msg.chat;
        let user_id: Result<i64, _> = user_id.0.try_into();
        if user_id != Ok(chat.id.0) {
            info!(
                "chat {} {} (@{} {})",
                chat.first_name().unwrap_or(""),
                chat.last_name().unwrap_or(""),
                chat.username().unwrap_or(""),
                chat.id.0
            );
        }
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
            return "Send an image, GIF, or sticker to convert.";
        } else {
            info!("invalid: {msg:#?}");
            return "Please send an image, GIF, or sticker.";
        };
        if size > MAX_SIZE {
            return "File is too large.";
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> AnyResult<()> {
    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var("RUST_LOG", "info");
        }
    }
    pretty_env_logger::init();

    check_command(FFMPEG, "-version")?;
    check_command(TGS_TO_GIF, "-v")?;
    check_command("gifski", "-V")?;
    check_command("gunzip", "--version")?;
    check_command("lottie_to_png", "-v")?;

    let bot = Bot::from_env();
    info!("bot started: {}", bot.get_my_name().await?.name);

    teloxide::repl(bot, |msg: Message, bot: Bot| async move {
        let chat_id = msg.chat.id;
        let msg_id = msg.id;
        let user_id = if let Some(user) = msg.from.as_ref() {
            info!(
                "from {} {} (@{} {})",
                user.first_name,
                user.last_name.as_deref().unwrap_or(""),
                user.username.as_deref().unwrap_or(""),
                user.id.0
            );
            user.id
        } else {
            info!("from unknown user");
            UserId(0)
        };

        spawn(async move {
            let resp_ids = RefCell::new(Vec::new());
            let msg_callback = |msg_id| {
                resp_ids.borrow_mut().push(msg_id);
            };

            let req = Request {
                msg,
                bot: bot.clone(),
                caption: None,
                base: None,
                msg_callback,
            };
            let s = req.handler(user_id).await;
            let mut resp_ids = resp_ids.into_inner();
            if !s.is_empty() {
                match bot
                    .send_message(chat_id, s)
                    .reply_parameters(ReplyParameters::new(msg_id))
                    .await
                {
                    Ok(msg) => {
                        resp_ids.push(msg.id);
                    }
                    Err(e) => {
                        error!("send_message: {e:?}");
                    }
                }
            }
            debug!("responded: {resp_ids:?}");
        });
        // TODO: join the spawned tasks when interrupted?
        Ok(())
    })
    .await;
    Ok(())
}
