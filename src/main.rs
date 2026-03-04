use clap::{Parser, Subcommand};
use email_address::EmailAddress;
use mailin_embedded::{Handler, Response, Server, response};
use mailparse::{MailHeaderMap, parse_mail};
use rand::RngExt as _;
use serde::Deserialize;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Deserialize, Clone)]
struct Config {
    #[serde(default)]
    host: String,
    domain: String,
    mail_path: String,
}

#[derive(Parser)]
#[command(name = "void")]
#[command(about = "Disposable email CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(help = "Inbox name to watch")]
    inbox: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    List,
    Serve {
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
        #[arg(long, default_value = "25")]
        port: u16,
        #[arg(long, env = "DOMAIN")]
        domain: Option<String>,
        #[arg(long, env = "MAIL_PATH", default_value = "/data")]
        mail_path: String,
    },
}

fn load_config() -> Config {
    let config_path = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("void")
        .join("config.toml");

    let content = std::fs::read_to_string(&config_path).unwrap_or_else(|_| {
        eprintln!("Config not found at {:?}", config_path);
        eprintln!("Create it with:");
        eprintln!("  host = \"your-ssh-host\"  # empty for local");
        eprintln!("  domain = \"void.example.com\"");
        eprintln!("  mail_path = \"/var/mail/vhosts/void.example.com\"");
        std::process::exit(1);
    });

    toml::from_str(&content).unwrap_or_else(|e| {
        eprintln!("Invalid config: {}", e);
        std::process::exit(1);
    })
}

fn is_local(config: &Config) -> bool {
    config.host.is_empty() || config.host == "localhost" || config.host == "127.0.0.1"
}

fn run_command(config: &Config, cmd: &str) -> Option<String> {
    let output = if is_local(config) {
        Command::new("sh").arg("-c").arg(cmd).output().ok()?
    } else {
        Command::new("ssh")
            .arg(&config.host)
            .arg(cmd)
            .output()
            .ok()?
    };

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

fn generate_inbox_name() -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::rng();
    (0..6)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

fn parse_inbox_arg(input: &str, config: &Config) -> String {
    let input = input.trim().to_lowercase();
    let email = if input.contains('@') {
        input
    } else {
        format!("{}@{}", input, config.domain)
    };

    match email.parse::<EmailAddress>() {
        Ok(addr) => {
            if addr.domain() != config.domain {
                eprintln!(
                    "Error: Wrong domain '{}', expected '{}'",
                    addr.domain(),
                    config.domain
                );
                std::process::exit(1);
            }
            addr.local_part().to_string()
        }
        Err(_) => {
            eprintln!("Error: Invalid email '{}'", email);
            std::process::exit(1);
        }
    }
}

fn copy_to_clipboard(text: &str) -> bool {
    use std::process::Stdio;
    Command::new("wl-copy")
        .arg(text)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

fn list_inboxes(config: &Config) {
    let cmd = format!("ls -1 '{}'", config.mail_path);
    let output = run_command(config, &cmd).unwrap_or_default();
    let inboxes: Vec<&str> = output.lines().collect();
    if inboxes.is_empty() {
        println!("No inboxes yet.");
    } else {
        for inbox in inboxes {
            println!("{}@{}", inbox, config.domain);
        }
    }
}

fn parse_email_file(content: &[u8]) -> Option<(String, String, String, String)> {
    let parsed = parse_mail(content).ok()?;
    let from = parsed.headers.get_first_value("From").unwrap_or_default();
    let date = parsed.headers.get_first_value("Date").unwrap_or_default();
    let subject = parsed
        .headers
        .get_first_value("Subject")
        .unwrap_or_default();

    let body = if parsed.subparts.is_empty() {
        let content_type = parsed
            .headers
            .get_first_value("Content-Type")
            .unwrap_or_default();
        let text = parsed.get_body().unwrap_or_default();
        if content_type.contains("text/html") {
            html2text::from_read(text.as_bytes(), 1000).unwrap_or_default()
        } else {
            text
        }
    } else {
        extract_body_from_parts(&parsed.subparts)
    };

    Some((from, date, subject, body))
}

fn extract_body_from_parts(parts: &[mailparse::ParsedMail]) -> String {
    let mut plain_text = String::new();
    let mut html_text = String::new();

    for part in parts {
        let content_type = part
            .headers
            .get_first_value("Content-Type")
            .unwrap_or_default();

        if content_type.contains("multipart/") {
            let nested = extract_body_from_parts(&part.subparts);
            if !nested.is_empty() && plain_text.is_empty() {
                plain_text = nested;
            }
        } else if content_type.contains("text/plain") && plain_text.is_empty() {
            plain_text = part.get_body().unwrap_or_default();
        } else if content_type.contains("text/html") && html_text.is_empty() {
            html_text = part.get_body().unwrap_or_default();
        }
    }

    if !plain_text.is_empty() {
        plain_text
    } else if !html_text.is_empty() {
        html2text::from_read(html_text.as_bytes(), 1000).unwrap_or_default()
    } else {
        String::new()
    }
}

fn watch_inbox(config: &Config, inbox: &str, show_copied: bool) {
    let maildir_path = format!("{}/{}/new", config.mail_path, inbox);
    let email_addr = format!("{}@{}", inbox, config.domain);

    let mut last_files: Vec<String> = Vec::new();
    let mut first_run = true;

    loop {
        let list_cmd = format!("ls -1t '{}' 2>/dev/null", maildir_path);
        let files: Vec<String> = run_command(config, &list_cmd)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect();

        if first_run || files != last_files {
            print!("\x1B[2J\x1B[1;1H");
            io::stdout().flush().ok();

            if first_run && show_copied {
                println!("Inbox: {} (copied)\n", email_addr);
            } else {
                println!("Inbox: {}\n", email_addr);
            }

            if files.is_empty() {
                println!("No emails yet...");
            } else {
                for (i, file) in files.iter().enumerate() {
                    let cat_cmd = format!("cat '{}/{}'", maildir_path, file);
                    if let Some(content) = run_command(config, &cat_cmd)
                        && let Some((from, date, subject, body)) =
                            parse_email_file(content.as_bytes())
                    {
                        println!("─────────────────────────────────────────");
                        println!("#{} | {} | {}", files.len() - i, date, from);
                        println!("Subject: {}", subject);
                        println!();
                        println!("{}", body.trim());
                        println!();
                    }
                }
            }

            last_files = files;
            first_run = false;
        }

        thread::sleep(Duration::from_secs(5));
    }
}

#[derive(Clone)]
struct SmtpHandler {
    mail_path: PathBuf,
    domain: String,
    current_recipients: Arc<Mutex<Vec<String>>>,
    current_data: Arc<Mutex<Vec<u8>>>,
}

impl SmtpHandler {
    fn new(mail_path: PathBuf, domain: String) -> Self {
        Self {
            mail_path,
            domain,
            current_recipients: Arc::new(Mutex::new(Vec::new())),
            current_data: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn generate_filename() -> String {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        let mut rng = rand::rng();
        let unique: u32 = rng.random();
        let hostname = gethostname::gethostname().to_string_lossy().to_string();
        format!("{}.{:08x}.{}", timestamp, unique, hostname)
    }

    fn save_email(&self, recipient: &str, data: &[u8]) -> io::Result<()> {
        let local_part = recipient
            .split('@')
            .next()
            .unwrap_or(recipient)
            .to_lowercase();

        let maildir = self.mail_path.join(&local_part);
        let tmp_dir = maildir.join("tmp");
        let new_dir = maildir.join("new");
        let cur_dir = maildir.join("cur");

        fs::create_dir_all(&tmp_dir)?;
        fs::create_dir_all(&new_dir)?;
        fs::create_dir_all(&cur_dir)?;

        let filename = Self::generate_filename();
        let tmp_path = tmp_dir.join(&filename);
        let new_path = new_dir.join(&filename);

        fs::write(&tmp_path, data)?;
        fs::rename(&tmp_path, &new_path)?;

        Ok(())
    }
}

impl Handler for SmtpHandler {
    fn helo(&mut self, _ip: std::net::IpAddr, _domain: &str) -> Response {
        response::OK
    }

    fn mail(&mut self, _ip: std::net::IpAddr, _domain: &str, _from: &str) -> Response {
        self.current_recipients.lock().unwrap().clear();
        response::OK
    }

    fn rcpt(&mut self, to: &str) -> Response {
        let to_lower = to.to_lowercase();
        if to_lower.ends_with(&format!("@{}", self.domain)) {
            self.current_recipients.lock().unwrap().push(to_lower);
            response::OK
        } else {
            response::NO_MAILBOX
        }
    }

    fn data_start(
        &mut self,
        _domain: &str,
        _from: &str,
        _is8bit: bool,
        _to: &[String],
    ) -> Response {
        self.current_data.lock().unwrap().clear();
        response::OK
    }

    fn data(&mut self, buf: &[u8]) -> io::Result<()> {
        self.current_data.lock().unwrap().extend_from_slice(buf);
        Ok(())
    }

    fn data_end(&mut self) -> Response {
        let data = self.current_data.lock().unwrap().clone();
        let recipients = self.current_recipients.lock().unwrap().clone();
        for recipient in recipients {
            if let Err(e) = self.save_email(&recipient, &data) {
                eprintln!("Failed to save email for {}: {}", recipient, e);
                return response::INTERNAL_ERROR;
            }
        }
        response::OK
    }
}

fn run_server(domain: &str, mail_path: &str, bind: &str, port: u16) {
    let handler = SmtpHandler::new(PathBuf::from(mail_path), domain.to_string());

    let addr = format!("{}:{}", bind, port);

    let mut server = Server::new(handler);
    server
        .with_addr(&addr)
        .expect("Invalid address")
        .with_name(domain);

    server.serve().expect("Failed to start server");
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::List) => {
            let config = load_config();
            list_inboxes(&config);
        }
        Some(Commands::Serve {
            bind,
            port,
            domain,
            mail_path,
        }) => {
            let domain = domain.unwrap_or_else(|| {
                eprintln!("Error: --domain or DOMAIN env var required");
                std::process::exit(1);
            });
            run_server(&domain, &mail_path, &bind, port);
        }
        None => {
            let config = load_config();
            let generated = cli.inbox.is_none();
            let inbox = cli
                .inbox
                .map(|s| parse_inbox_arg(&s, &config))
                .unwrap_or_else(|| {
                    let existing_cmd = format!("ls -1 '{}'", config.mail_path);
                    let existing: Vec<String> = run_command(&config, &existing_cmd)
                        .unwrap_or_default()
                        .lines()
                        .map(String::from)
                        .collect();

                    loop {
                        let candidate = generate_inbox_name();
                        if !existing.contains(&candidate) {
                            let email = format!("{}@{}", candidate, config.domain);
                            copy_to_clipboard(&email);
                            break candidate;
                        }
                    }
                });

            watch_inbox(&config, &inbox, generated);
        }
    }
}
