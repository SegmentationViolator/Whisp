use std::{
    cell::RefCell,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use gio::ApplicationFlags;
use glib::{ControlFlow, SourceId};
use gtk::{
    Application, ApplicationWindow, Box as GtkBox, CssProvider, Frame, Label, Orientation,
    ProgressBar, STYLE_PROVIDER_PRIORITY_APPLICATION, gdk, prelude::*,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use serde::{Deserialize, Serialize};

const APP_ID: &str = "dev.whisp.Osd";
const DEFAULT_TIMEOUT_MS: u64 = 1_200;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Wayland-native OSD daemon for volume and brightness"
)]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Daemon {
        #[arg(long, default_value_t = DEFAULT_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    Show {
        kind: MeterKind,
        value: f64,
        #[arg(long, default_value_t = 100.0)]
        max: f64,
        #[arg(long)]
        muted: bool,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
enum MeterKind {
    Volume,
    Brightness,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OsdMessage {
    kind: MeterKind,
    value: f64,
    max: f64,
    muted: bool,
    label: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Clone)]
struct UiState {
    window: ApplicationWindow,
    badge: Label,
    title: Label,
    value: Label,
    bar: ProgressBar,
    hide_source: Rc<RefCell<Option<SourceId>>>,
}

impl UiState {
    fn new(app: &Application) -> Result<Self> {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Whisp")
            .default_width(320)
            .default_height(104)
            .resizable(false)
            .decorated(false)
            .build();

        window.init_layer_shell();
        window.set_namespace(Some("whisp"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Right, true);
        window.set_margin(Edge::Top, 24);
        window.set_margin(Edge::Right, 24);
        window.set_focusable(false);
        window.add_css_class("whisp-window");

        let frame = Frame::new(None);
        frame.add_css_class("whisp-frame");

        let root = GtkBox::new(Orientation::Horizontal, 16);
        root.set_margin_top(18);
        root.set_margin_bottom(18);
        root.set_margin_start(18);
        root.set_margin_end(18);

        let badge = Label::new(Some("VOL"));
        badge.add_css_class("whisp-badge");
        badge.set_width_chars(4);
        badge.set_xalign(0.5);

        let body = GtkBox::new(Orientation::Vertical, 10);
        body.set_hexpand(true);

        let title_row = GtkBox::new(Orientation::Horizontal, 12);
        let title = Label::new(Some("Volume"));
        title.add_css_class("whisp-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);

        let value = Label::new(Some("0%"));
        value.add_css_class("whisp-value");
        value.set_xalign(1.0);

        title_row.append(&title);
        title_row.append(&value);

        let bar = ProgressBar::new();
        bar.add_css_class("whisp-bar");
        bar.set_fraction(0.0);
        bar.set_hexpand(true);
        bar.set_show_text(false);

        body.append(&title_row);
        body.append(&bar);

        root.append(&badge);
        root.append(&body);
        frame.set_child(Some(&root));
        window.set_child(Some(&frame));
        window.set_visible(false);

        install_css()?;

        Ok(Self {
            window,
            badge,
            title,
            value,
            bar,
            hide_source: Rc::new(RefCell::new(None)),
        })
    }

    fn show_message(&self, message: OsdMessage, default_timeout_ms: u64) {
        let max = if message.max <= 0.0 {
            100.0
        } else {
            message.max
        };
        let clamped = message.value.clamp(0.0, max);
        let fraction = (clamped / max).clamp(0.0, 1.0);

        self.badge.set_text(match message.kind {
            MeterKind::Volume => "VOL",
            MeterKind::Brightness => "BRT",
        });

        self.title
            .set_text(message.label.as_deref().unwrap_or(match message.kind {
                MeterKind::Volume => "Volume",
                MeterKind::Brightness => "Brightness",
            }));

        if matches!(message.kind, MeterKind::Volume) && message.muted {
            self.value.set_text("Muted");
            self.bar.set_fraction(0.0);
            self.bar.add_css_class("muted");
        } else {
            self.value.set_text(&format!("{:.0}%", fraction * 100.0));
            self.bar.set_fraction(fraction);
            self.bar.remove_css_class("muted");
        }

        self.window.present();
        self.window.set_visible(true);

        if let Some(source_id) = self.hide_source.borrow_mut().take() {
            source_id.remove();
        }

        let window = self.window.clone();
        let timeout = Duration::from_millis(message.timeout_ms.unwrap_or(default_timeout_ms));
        let source_id = glib::timeout_add_local_once(timeout, move || {
            window.set_visible(false);
        });
        self.hide_source.borrow_mut().replace(source_id);
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("whisp: {error:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let socket_path = cli.socket.unwrap_or(default_socket_path()?);

    match cli.command {
        Command::Daemon { timeout_ms } => run_daemon(socket_path, timeout_ms),
        Command::Show {
            kind,
            value,
            max,
            muted,
            label,
            timeout_ms,
        } => send_message(
            &socket_path,
            &OsdMessage {
                kind,
                value,
                max,
                muted,
                label,
                timeout_ms,
            },
        ),
    }
}

fn run_daemon(socket_path: PathBuf, timeout_ms: u64) -> Result<()> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        bail!("WAYLAND_DISPLAY is not set; Whisp only runs on Wayland");
    }

    // SAFETY: This happens before GTK initialization and before any threads are started.
    unsafe {
        std::env::set_var("GDK_BACKEND", "wayland");
    }

    let app = Application::builder()
        .application_id(APP_ID)
        .flags(ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_activate(move |app| {
        if let Err(error) = activate(app, socket_path.clone(), timeout_ms) {
            eprintln!("whisp: {error:?}");
            app.quit();
        }
    });
    app.run();
    Ok(())
}

fn activate(app: &Application, socket_path: PathBuf, timeout_ms: u64) -> Result<()> {
    if !gtk4_layer_shell::is_supported() {
        bail!("layer-shell is not supported by this compositor");
    }

    let ui = Rc::new(UiState::new(app)?);
    let (sender, receiver) = mpsc::channel::<OsdMessage>();

    start_socket_listener(socket_path.clone(), sender)?;

    {
        let ui = ui.clone();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            while let Ok(message) = receiver.try_recv() {
                ui.show_message(message, timeout_ms);
            }
            ControlFlow::Continue
        });
    }

    app.connect_shutdown(move |_| {
        let _ = fs::remove_file(&socket_path);
    });

    Ok(())
}

fn start_socket_listener(socket_path: PathBuf, sender: mpsc::Sender<OsdMessage>) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create socket directory {}", parent.display()))?;
    }

    remove_stale_socket(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket at {}", socket_path.display()))?;

    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    if let Err(error) = handle_stream(stream, &sender) {
                        eprintln!("whisp: failed to handle socket message: {error:?}");
                    }
                }
                Err(error) => {
                    eprintln!("whisp: socket listener error: {error}");
                    break;
                }
            }
        }
    });

    Ok(())
}

fn handle_stream(stream: UnixStream, sender: &mpsc::Sender<OsdMessage>) -> Result<()> {
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("failed to read from client socket")?;

    if line.trim().is_empty() {
        return Ok(());
    }

    let message =
        serde_json::from_str::<OsdMessage>(&line).context("failed to decode OSD payload")?;
    sender
        .send(message)
        .map_err(|_| anyhow!("OSD UI thread is not available"))?;
    Ok(())
}

fn send_message(socket_path: &Path, message: &OsdMessage) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("failed to connect to socket at {}", socket_path.display()))?;
    let mut payload = serde_json::to_vec(message).context("failed to serialize OSD payload")?;
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .context("failed to write OSD payload to socket")?;
    Ok(())
}

fn remove_stale_socket(socket_path: &Path) -> Result<()> {
    if !socket_path.exists() {
        return Ok(());
    }

    match UnixStream::connect(socket_path) {
        Ok(_) => bail!(
            "socket {} is already in use; is another Whisp daemon running?",
            socket_path.display()
        ),
        Err(_) => {
            fs::remove_file(socket_path).with_context(|| {
                format!("failed to remove stale socket {}", socket_path.display())
            })?;
        }
    }

    Ok(())
}

fn default_socket_path() -> Result<PathBuf> {
    let runtime_dir =
        std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("whisp.sock"))
}

fn install_css() -> Result<()> {
    let provider = CssProvider::new();
    provider.load_from_data(
        r#"
        .whisp-window {
            background: transparent;
        }

        .whisp-frame {
            background: rgba(20, 24, 32, 0.92);
            border-radius: 20px;
            border: 1px solid rgba(255, 255, 255, 0.08);
            box-shadow: 0 18px 40px rgba(0, 0, 0, 0.28);
        }

        .whisp-badge {
            background: rgba(116, 185, 255, 0.16);
            color: #d8ebff;
            border-radius: 999px;
            padding: 12px 10px;
            font-weight: 700;
            letter-spacing: 0.08em;
        }

        .whisp-title {
            color: #f4f7fb;
            font-size: 1.1rem;
            font-weight: 700;
        }

        .whisp-value {
            color: #b9c2cf;
            font-weight: 600;
        }

        .whisp-bar trough {
            background: rgba(255, 255, 255, 0.08);
            border-radius: 999px;
            min-height: 12px;
        }

        .whisp-bar progress {
            background: linear-gradient(90deg, #5fb0ff 0%, #8be9c6 100%);
            border-radius: 999px;
            min-height: 12px;
        }

        .whisp-bar.muted progress {
            background: rgba(255, 255, 255, 0.14);
        }
        "#,
    );

    let display = gdk::Display::default().ok_or_else(|| anyhow!("failed to get GDK display"))?;
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    Ok(())
}
