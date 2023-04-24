use std::io::{BufRead, BufReader};

use crate::{
    lang::TRANSLATOR,
    prelude::{run_command, CommandError, CommandOutput, Error, Finality, Privacy, StrictPath, SyncDirection},
    resource::config::{App, Config},
    scan::ScanChange,
};

pub fn validate_cloud_config(config: &Config, cloud_path: &str) -> Result<Remote, Error> {
    if !config.apps.rclone.is_valid() {
        return Err(Error::RcloneUnavailable);
    }
    let Some(remote) = config.cloud.remote.clone() else { return Err(Error::CloudNotConfigured) };
    validate_cloud_path(cloud_path)?;
    Ok(remote)
}

pub fn validate_cloud_path(path: &str) -> Result<(), Error> {
    if path.is_empty() || path == "/" {
        Err(Error::CloudPathInvalid)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct CloudChange {
    pub path: String,
    pub change: ScanChange,
}

#[derive(Clone, Debug)]
pub enum RcloneProcessEvent {
    Progress { current: f32, max: f32 },
    Change(CloudChange),
}

#[derive(Debug)]
pub struct RcloneProcess {
    program: String,
    args: Vec<String>,
    child: std::process::Child,
    stderr: Option<BufReader<std::process::ChildStderr>>,
}

impl RcloneProcess {
    pub fn launch(program: String, args: Vec<String>) -> Result<Self, CommandError> {
        let mut command = std::process::Command::new(&program);
        command
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);
        }

        log::debug!("Running command: {} {:?}", &program, &args);

        let mut child = command.spawn().map_err(|e| {
            let e = CommandError::Launched {
                program: program.clone(),
                args: args.clone(),
                raw: e.to_string(),
            };
            log::error!("Rclone failed: {e:?}");
            e
        })?;

        let stderr = child.stderr.take().map(BufReader::new);
        Ok(Self {
            program,
            args,
            child,
            stderr,
        })
    }

    pub fn events(&mut self) -> Vec<RcloneProcessEvent> {
        let mut events = vec![];

        #[derive(Debug, serde::Deserialize)]
        #[serde(rename_all = "camelCase", untagged)]
        enum Log {
            Skip { skipped: String, object: String },
            Change { msg: String, object: String },
            Stats { stats: Stats },
        }

        #[derive(Debug, serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Stats {
            bytes: f32,
            total_bytes: f32,
        }

        if let Some(stderr) = self.stderr.as_mut() {
            for line in stderr.lines().take(10).filter_map(|x| x.ok()) {
                match serde_json::from_str::<Log>(&line) {
                    Ok(Log::Skip { skipped, object }) => match skipped.as_str() {
                        "copy" => events.push(RcloneProcessEvent::Change(CloudChange {
                            path: object,
                            change: ScanChange::Different,
                        })),
                        "delete" => events.push(RcloneProcessEvent::Change(CloudChange {
                            path: object,
                            change: ScanChange::Removed,
                        })),
                        raw => {
                            log::trace!("Unhandled Rclone 'skipped': {raw}");
                        }
                    },
                    Ok(Log::Change { msg, object }) => match msg.as_str() {
                        "Copied (new)" => events.push(RcloneProcessEvent::Change(CloudChange {
                            path: object,
                            change: ScanChange::New,
                        })),
                        "Copied (replaced existing)" => events.push(RcloneProcessEvent::Change(CloudChange {
                            path: object,
                            change: ScanChange::Different,
                        })),
                        "Deleted" => events.push(RcloneProcessEvent::Change(CloudChange {
                            path: object,
                            change: ScanChange::Removed,
                        })),
                        raw => {
                            log::trace!("Unhandled Rclone 'msg': {raw}");
                        }
                    },
                    Ok(Log::Stats {
                        stats: Stats { bytes, total_bytes },
                    }) => {
                        events.push(RcloneProcessEvent::Progress {
                            current: bytes,
                            max: total_bytes,
                        });
                    }
                    Err(_) => {
                        log::trace!("Unhandled Rclone message: {line}");
                    }
                }
            }
        }

        if !events.is_empty() {
            log::trace!("New Rclone events: {events:?}");
        }
        events
    }

    pub fn succeeded(&mut self) -> Option<Result<(), CommandError>> {
        let res = match self.child.try_wait() {
            Ok(Some(status)) => match status.code() {
                Some(code) if code == 0 => Some(Ok(())),
                Some(code) => {
                    let stdout = self.child.stdout.as_mut().and_then(|x| {
                        let lines = BufReader::new(x).lines().filter_map(|x| x.ok()).collect::<Vec<_>>();
                        (!lines.is_empty()).then_some(lines.join("\n"))
                    });
                    let stderr = self.stderr.as_mut().and_then(|x| {
                        let lines = x.lines().filter_map(|x| x.ok()).collect::<Vec<_>>();
                        (!lines.is_empty()).then_some(lines.join("\n"))
                    });

                    Some(Err(CommandError::Exited {
                        program: self.program.clone(),
                        args: self.args.clone(),
                        code,
                        stdout,
                        stderr,
                    }))
                }
                None => Some(Err(CommandError::Terminated {
                    program: self.program.clone(),
                    args: self.args.clone(),
                })),
            },
            Ok(None) => None,
            Err(_) => Some(Err(CommandError::Terminated {
                program: self.program.clone(),
                args: self.args.clone(),
            })),
        };

        if let Some(Ok(_)) = &res {
            log::debug!("Rclone succeeded");
        }
        if let Some(Err(e)) = &res {
            log::error!("Rclone failed: {e:?}");
        }

        res
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename = "camelCase")]
pub enum RemoteChoice {
    None,
    Custom,
    Box,
    Dropbox,
    Ftp,
    GoogleDrive,
    OneDrive,
    Smb,
    WebDav,
}

impl RemoteChoice {
    pub const ALL: &[Self] = &[
        Self::None,
        Self::Box,
        Self::Dropbox,
        Self::GoogleDrive,
        Self::OneDrive,
        Self::Ftp,
        Self::Smb,
        Self::WebDav,
        Self::Custom,
    ];
}

impl ToString for RemoteChoice {
    fn to_string(&self) -> String {
        match self {
            Self::None => TRANSLATOR.none_label(),
            Self::Custom => TRANSLATOR.custom_label(),
            Self::Box => "Box".to_string(),
            Self::Dropbox => "Dropbox".to_string(),
            Self::Ftp => "FTP".to_string(),
            Self::GoogleDrive => "Google Drive".to_string(),
            Self::OneDrive => "OneDrive".to_string(),
            Self::Smb => "SMB".to_string(),
            Self::WebDav => "WebDAV".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename = "camelCase")]
pub enum Remote {
    Custom {
        name: String,
    },
    Box,
    Dropbox,
    GoogleDrive,
    OneDrive,
    Ftp {
        host: String,
        port: i32,
        username: String,
        #[serde(skip, default)]
        password: String,
    },
    Smb {
        host: String,
        port: i32,
        username: String,
        #[serde(skip, default)]
        password: String,
    },
    WebDav {
        url: String,
        username: String,
        #[serde(skip, default)]
        password: String,
        provider: WebDavProvider,
    },
}

impl Remote {
    pub fn name(&self) -> &str {
        match self {
            Self::Custom { name } => name,
            _ => "ludusavi",
        }
    }

    pub fn slug(&self) -> &str {
        match self {
            Self::Custom { .. } => "",
            Self::Box => "box",
            Self::Dropbox => "dropbox",
            Self::Ftp { .. } => "ftp",
            Self::GoogleDrive => "drive",
            Self::OneDrive => "onedrive",
            Self::Smb { .. } => "smb",
            Self::WebDav { .. } => "webdav",
        }
    }

    pub fn config_args(&self) -> Option<Vec<String>> {
        match self {
            Self::Custom { .. } => None,
            Self::Box => None,
            Self::Dropbox => None,
            Self::GoogleDrive => Some(vec!["scope=drive".to_string()]),
            Self::Ftp {
                host,
                port,
                username,
                password,
            } => Some(vec![
                format!("host={host}"),
                format!("port={port}"),
                format!("user={username}"),
                format!("pass={password}"),
            ]),
            Self::OneDrive => Some(vec![
                "drive_type=personal".to_string(),
                "access_scopes=Files.ReadWrite,offline_access".to_string(),
            ]),
            Self::Smb {
                host,
                port,
                username,
                password,
            } => Some(vec![
                format!("host={host}"),
                format!("port={port}"),
                format!("user={username}"),
                format!("pass={password}"),
            ]),
            Self::WebDav {
                url,
                username,
                password,
                provider,
            } => Some(vec![
                format!("url={url}"),
                format!("user={username}"),
                format!("pass={password}"),
                format!("vendor={}", provider.slug()),
            ]),
        }
    }

    pub fn needs_configuration(&self) -> bool {
        match self {
            Self::Custom { .. } => false,
            Self::Box
            | Self::Dropbox
            | Self::Ftp { .. }
            | Self::GoogleDrive
            | Self::OneDrive
            | Self::Smb { .. }
            | Self::WebDav { .. } => true,
        }
    }

    pub fn description(&self) -> Option<String> {
        match self {
            Remote::Ftp {
                host, port, username, ..
            } => Some(format!("{}@{}:{}", username, host, port)),
            Remote::Smb {
                host, port, username, ..
            } => Some(format!("{}@{}:{}", username, host, port)),
            Remote::WebDav { url, provider, .. } => Some(format!("{} - {}", provider.to_string(), url)),
            _ => None,
        }
    }
}

impl From<Option<&Remote>> for RemoteChoice {
    fn from(value: Option<&Remote>) -> Self {
        if let Some(value) = value {
            match value {
                Remote::Custom { .. } => RemoteChoice::Custom,
                Remote::Box => RemoteChoice::Box,
                Remote::Dropbox => RemoteChoice::Dropbox,
                Remote::Ftp { .. } => RemoteChoice::Ftp,
                Remote::GoogleDrive => RemoteChoice::GoogleDrive,
                Remote::OneDrive => RemoteChoice::OneDrive,
                Remote::Smb { .. } => RemoteChoice::Smb,
                Remote::WebDav { .. } => RemoteChoice::WebDav,
            }
        } else {
            RemoteChoice::None
        }
    }
}

impl TryFrom<RemoteChoice> for Remote {
    type Error = ();

    fn try_from(value: RemoteChoice) -> Result<Self, Self::Error> {
        match value {
            RemoteChoice::None => Err(()),
            RemoteChoice::Custom => Ok(Remote::Custom {
                name: "ludusavi".to_string(),
            }),
            RemoteChoice::Box => Ok(Remote::Box),
            RemoteChoice::Dropbox => Ok(Remote::Dropbox),
            RemoteChoice::Ftp => Ok(Remote::Ftp {
                host: String::new(),
                port: 21,
                username: String::new(),
                password: String::new(),
            }),
            RemoteChoice::GoogleDrive => Ok(Remote::GoogleDrive),
            RemoteChoice::OneDrive => Ok(Remote::OneDrive),
            RemoteChoice::Smb => Ok(Remote::Smb {
                host: String::new(),
                port: 445,
                username: String::new(),
                password: String::new(),
            }),
            RemoteChoice::WebDav => Ok(Remote::WebDav {
                url: String::new(),
                username: String::new(),
                password: String::new(),
                provider: WebDavProvider::Other,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WebDavProvider {
    #[default]
    Other,
    Nextcloud,
    Owncloud,
    Sharepoint,
    SharepointNtlm,
}

impl WebDavProvider {
    pub const ALL: &[Self] = &[
        Self::Other,
        Self::Nextcloud,
        Self::Owncloud,
        Self::Sharepoint,
        Self::SharepointNtlm,
    ];

    pub const ALL_CLI: &[&'static str] = &[
        Self::OTHER,
        Self::NEXTCLOUD,
        Self::OWNCLOUD,
        Self::SHAREPOINT,
        Self::SHAREPOINT_NTLM,
    ];
    pub const OTHER: &str = "other";
    const NEXTCLOUD: &str = "nextcloud";
    const OWNCLOUD: &str = "owncloud";
    const SHAREPOINT: &str = "sharepoint";
    const SHAREPOINT_NTLM: &str = "sharepoint-ntlm";
}

impl WebDavProvider {
    pub fn slug(&self) -> &str {
        match self {
            WebDavProvider::Other => Self::OTHER,
            WebDavProvider::Nextcloud => Self::NEXTCLOUD,
            WebDavProvider::Owncloud => Self::OWNCLOUD,
            WebDavProvider::Sharepoint => Self::SHAREPOINT,
            WebDavProvider::SharepointNtlm => Self::SHAREPOINT_NTLM,
        }
    }
}

impl ToString for WebDavProvider {
    fn to_string(&self) -> String {
        match self {
            Self::Other => crate::resource::manifest::Store::Other.to_string(),
            Self::Nextcloud => "Nextcloud".to_string(),
            Self::Owncloud => "Owncloud".to_string(),
            Self::Sharepoint => "Sharepoint".to_string(),
            Self::SharepointNtlm => "Sharepoint (NTLM)".to_string(),
        }
    }
}

impl std::str::FromStr for WebDavProvider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            Self::OTHER => Ok(Self::Other),
            Self::NEXTCLOUD => Ok(Self::Nextcloud),
            Self::OWNCLOUD => Ok(Self::Owncloud),
            Self::SHAREPOINT => Ok(Self::Sharepoint),
            Self::SHAREPOINT_NTLM => Ok(Self::SharepointNtlm),
            _ => Err(format!("invalid provider: {}", s)),
        }
    }
}

pub struct Rclone {
    app: App,
    remote: Remote,
}

impl Rclone {
    pub fn new(app: App, remote: Remote) -> Self {
        Self { app, remote }
    }

    fn path(&self, path: &str) -> String {
        format!("{}:{}", self.remote.name(), path)
    }

    fn args(&self, args: &[String]) -> Vec<String> {
        let mut collected = vec![];
        if !self.app.arguments.is_empty() {
            if let Some(parts) = shlex::split(&self.app.arguments) {
                collected.extend(parts);
            }
        }
        for arg in args {
            collected.push(arg.to_string());
        }
        collected
    }

    fn run(&self, args: &[String], success: &[i32], privacy: Privacy) -> Result<CommandOutput, CommandError> {
        let args = self.args(args);
        let args: Vec<_> = args.iter().map(|x| x.as_str()).collect();
        run_command(&self.app.path.raw(), &args, success, privacy)
    }

    fn obscure(&self, credential: &str) -> Result<String, CommandError> {
        let out = self.run(&["obscure".to_string(), credential.to_string()], &[0], Privacy::Private)?;
        Ok(out.stdout)
    }

    pub fn configure_remote(&self) -> Result<(), CommandError> {
        if !self.remote.needs_configuration() {
            return Ok(());
        }

        let mut privacy = Privacy::Public;

        let mut remote = self.remote.clone();
        match &mut remote {
            Remote::Custom { .. } | Remote::Box | Remote::Dropbox | Remote::GoogleDrive | Remote::OneDrive => {}
            Remote::Ftp { password, .. } => {
                privacy = Privacy::Private;
                *password = self.obscure(password)?;
            }
            Remote::Smb { password, .. } => {
                privacy = Privacy::Private;
                *password = self.obscure(password)?;
            }
            Remote::WebDav { password, .. } => {
                privacy = Privacy::Private;
                *password = self.obscure(password)?;
            }
        }

        let mut args = vec![
            "config".to_string(),
            "create".to_string(),
            remote.name().to_string(),
            remote.slug().to_string(),
        ];

        if let Some(config_args) = remote.config_args() {
            args.extend(config_args);
        }

        self.run(&args, &[0], privacy)?;
        Ok(())
    }

    pub fn sync(
        &self,
        local: &StrictPath,
        remote_path: &str,
        direction: SyncDirection,
        finality: Finality,
        game_dirs: &[String],
    ) -> Result<RcloneProcess, CommandError> {
        let mut args = vec![
            "sync".to_string(),
            "-v".to_string(),
            "--use-json-log".to_string(),
            "--stats=100ms".to_string(),
        ];

        if finality.preview() {
            args.push("--dry-run".to_string());
        }

        for game_dir in game_dirs {
            // Inclusion rules are file-based, so we have to add `**`.
            args.push(format!("--include=/{game_dir}/**"));
        }

        match direction {
            SyncDirection::Upload => {
                args.push(local.render());
                args.push(self.path(remote_path));
            }
            SyncDirection::Download => {
                args.push(self.path(remote_path));
                args.push(local.render());
            }
        }

        RcloneProcess::launch(self.app.path.raw(), self.args(&args))
    }
}

pub mod rclone_monitor {
    use iced_native::{
        futures::{channel::mpsc, StreamExt},
        subscription::{self, Subscription},
    };

    use crate::{
        cloud::{RcloneProcess, RcloneProcessEvent},
        prelude::CommandError,
    };

    #[derive(Debug, Clone)]
    pub enum Event {
        Ready(mpsc::Sender<Input>),
        Data(Vec<RcloneProcessEvent>),
        Succeeded,
        Failed(CommandError),
        Cancelled,
    }

    #[derive(Debug)]
    pub enum Input {
        Process(RcloneProcess),
        Tick,
        Cancel,
    }

    enum State {
        Starting,
        Ready {
            receiver: mpsc::Receiver<Input>,
            process: Option<RcloneProcess>,
            interval: tokio::time::Interval,
        },
    }

    pub fn run() -> Subscription<Event> {
        struct Runner;

        subscription::unfold(std::any::TypeId::of::<Runner>(), State::Starting, |state| async move {
            match state {
                State::Starting => {
                    let (sender, receiver) = mpsc::channel(10_000);

                    (
                        Some(Event::Ready(sender)),
                        State::Ready {
                            receiver,
                            process: None,
                            interval: tokio::time::interval(std::time::Duration::from_millis(1)),
                        },
                    )
                }
                State::Ready {
                    mut receiver,
                    mut process,
                    mut interval,
                } => {
                    let input = tokio::select!(
                        input = receiver.select_next_some() => {
                            input
                        }
                        _ = interval.tick() => {
                            Input::Tick
                        }
                    );

                    match input {
                        Input::Process(new_process) => {
                            if let Some(proc) = process.as_mut() {
                                let _ = proc.child.kill();
                            }
                            process = Some(new_process);
                            (
                                None,
                                State::Ready {
                                    receiver,
                                    process,
                                    interval,
                                },
                            )
                        }
                        Input::Tick => {
                            if let Some(proc) = process.as_mut() {
                                let events = proc.events();
                                if !events.is_empty() {
                                    return (
                                        Some(Event::Data(events)),
                                        State::Ready {
                                            receiver,
                                            process,
                                            interval,
                                        },
                                    );
                                }
                                if let Some(outcome) = proc.succeeded() {
                                    match outcome {
                                        Ok(_) => {
                                            return (
                                                Some(Event::Succeeded),
                                                State::Ready {
                                                    receiver,
                                                    process: None,
                                                    interval,
                                                },
                                            );
                                        }
                                        Err(e) => {
                                            return (
                                                Some(Event::Failed(e)),
                                                State::Ready {
                                                    receiver,
                                                    process: None,
                                                    interval,
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                            (
                                None,
                                State::Ready {
                                    receiver,
                                    process,
                                    interval,
                                },
                            )
                        }
                        Input::Cancel => {
                            if let Some(proc) = process.as_mut() {
                                let _ = proc.child.kill();
                            }
                            (
                                Some(Event::Cancelled),
                                State::Ready {
                                    receiver,
                                    process: None,
                                    interval,
                                },
                            )
                        }
                    }
                }
            }
        })
    }
}
