#[cfg(not(unix))]
compile_error!("spacy-runner requires a Unix target");

#[cfg(unix)]
fn main() -> impl std::process::Termination {
    unix::run()
}

#[cfg(unix)]
mod unix {
    use clap::builder::OsStringValueParser;
    use clap::parser::{MatchesError, RawValues};
    use clap::{Arg, ArgMatches, Command as Cli};

    use const_format::concatcp;

    use std::borrow::Cow;
    use std::ffi::{OsStr, c_int};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::path::{self, Path};
    use std::process::{Command, ExitCode, ExitStatus, Stdio, Termination};
    use std::{env, fmt, io};

    const PYTHON_VERSION: &str = "3.14";
    const SPACY_VERSION: &str = "3.8.18";
    const PYTHON_REQUEST: &str = concatcp!("cpython@", PYTHON_VERSION);
    const SPACY_REQUIREMENT: &str = concatcp!("spacy==", SPACY_VERSION);

    pub(super) fn run() -> impl Termination {
        Outcome(run_impl())
    }

    fn cli() -> Cli {
        Cli::new("spacy-runner")
            .subcommand_required(true)
            .arg(
                Arg::new("uv")
                    .short('u')
                    .long("uv")
                    .env("UV_BIN")
                    .default_value("uv")
                    .value_name("BIN")
                    .value_parser(OsStringValueParser::new()),
            )
            .arg(
                Arg::new("venv")
                    .short('v')
                    .long("venv")
                    .env("VIRTUAL_ENV")
                    .default_value(".venv")
                    .value_name("DIR")
                    .value_parser(OsStringValueParser::new()),
            )
            .arg(
                Arg::new("python")
                    .short('p')
                    .long("python")
                    .env("PYTHON_VERSION")
                    .default_value(PYTHON_VERSION)
                    .value_name("VERSION")
                    .value_parser(OsStringValueParser::new()),
            )
            .arg(
                Arg::new("spacy")
                    .short('s')
                    .long("spacy")
                    .env("SPACY_VERSION")
                    .default_value(SPACY_VERSION)
                    .value_name("VERSION")
                    .value_parser(OsStringValueParser::new()),
            )
            .subcommand(
                Cli::new("install").visible_alias("i").arg(
                    Arg::new("model")
                        .value_name("MODEL")
                        .required(true)
                        .value_parser(OsStringValueParser::new()),
                ),
            )
            .subcommand(
                Cli::new("exec").visible_alias("x").arg(
                    Arg::new("command")
                        .value_name("PROG")
                        .num_args(1..)
                        .required(true)
                        .trailing_var_arg(true)
                        .allow_hyphen_values(true)
                        .value_parser(OsStringValueParser::new()),
                ),
            )
    }

    struct Config<'a> {
        uv: &'a OsStr,
        venv: &'a Path,
        python: &'a str,
        spacy: &'a str,
    }

    impl<'a> Config<'a> {
        fn parse(args: &'a ArgMatches) -> Result<Self, Error> {
            let uv = argument(args, "uv")?;
            let venv = argument(args, "venv")?;
            let python = argument(args, "python")?
                .to_str()
                .ok_or(Error::Invalid("Python version must be valid UTF-8"))?;
            let spacy = argument(args, "spacy")?
                .to_str()
                .ok_or(Error::Invalid("spaCy version must be valid UTF-8"))?;
            if uv.is_empty() || venv.is_empty() {
                return Err(Error::Invalid("uv binary and venv path must not be empty"));
            }
            if spacy.is_empty() {
                return Err(Error::Invalid("spaCy version must not be empty"));
            }

            let mut parts = python.split('.');
            let valid_part = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
            if !parts.next().is_some_and(valid_part)
                || !parts.next().is_some_and(valid_part)
                || parts.next().is_some_and(|s| !valid_part(s))
                || parts.next().is_some()
            {
                return Err(Error::Invalid(
                    "Python version must have the form major.minor[.patch]",
                ));
            }
            Ok(Self {
                uv,
                venv: Path::new(venv),
                python,
                spacy,
            })
        }
    }

    fn argument<'a>(args: &'a ArgMatches, name: &'static str) -> Result<&'a OsStr, Error> {
        args.try_get_raw(name)
            .map_err(Error::Matches)?
            .and_then(|mut args| args.next())
            .ok_or(Error::Missing(name))
    }

    enum Action<'a> {
        Install(&'a OsStr),
        Exec {
            prog: &'a OsStr,
            args: RawValues<'a>,
        },
    }

    impl<'a> Action<'a> {
        fn parse(args: &'a ArgMatches) -> Result<Self, Error> {
            match args.subcommand() {
                Some(("install", args)) => {
                    let model = argument(args, "model")?;
                    if model.is_empty() {
                        return Err(Error::Invalid("model must not be empty"));
                    }
                    Ok(Self::Install(model))
                }
                Some(("exec", args)) => {
                    let mut args = args
                        .try_get_raw("command")
                        .map_err(Error::Matches)?
                        .ok_or(Error::Missing("command"))?;
                    let prog = args.next().ok_or(Error::Missing("program"))?;
                    if prog.is_empty() {
                        return Err(Error::Invalid("program must not be empty"));
                    }
                    Ok(Self::Exec { prog, args })
                }
                _ => Err(Error::Invalid("missing install or exec command")),
            }
        }
    }

    fn run_impl() -> Result<(), Error> {
        let args = cli().try_get_matches().map_err(Error::Arguments)?;
        let config = Config::parse(&args)?;
        let action = Action::parse(&args)?;

        let mut python =
            path::absolute(config.venv).map_err(|e| Error::Io("resolve venv path", e))?;
        python.push("bin/python");
        let bin = python
            .parent()
            .ok_or(Error::Invalid("missing venv bin directory"))?;
        let venv = bin
            .parent()
            .ok_or(Error::Invalid("missing venv directory"))?;
        provision(&config, venv)?;

        eprintln!(
            "Reading Python library and package paths from {}",
            python.display()
        );
        let metadata = python_metadata(&python)?;
        let mut fields = metadata.split(|&c| c == 0);
        let (Some(lib), Some(modules), Some([]), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(Error::Invalid("invalid paths returned by Python"));
        };
        if lib.is_empty() || modules.is_empty() {
            return Err(Error::Invalid(
                "Python returned an empty library or package path",
            ));
        }

        let mut command = match action {
            Action::Install(model) => {
                let mut command = Command::new(&python);
                command.args(["-m", "spacy", "download"]).arg(model);
                command
            }
            Action::Exec { prog, args } => {
                let mut command = Command::new(prog);
                command.args(args);
                command
            }
        };
        command
            // PYTHONHOME overrides virtual-environment detection, including pip's target.
            .env_remove("PYTHONHOME")
            .env("VIRTUAL_ENV", venv)
            .env("PYO3_PYTHON", &python)
            .env("UV_PYTHON", &python)
            .env("UV_PROJECT_ENVIRONMENT", venv)
            .env("UV_NO_PROJECT", "1")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        prepend_env(&mut command, "PATH", bin.as_os_str());
        prepend_env(&mut command, "PYTHONPATH", OsStr::from_bytes(modules));
        prepend_env(&mut command, "LD_LIBRARY_PATH", OsStr::from_bytes(lib));
        #[cfg(target_os = "macos")]
        prepend_env(&mut command, "DYLD_LIBRARY_PATH", OsStr::from_bytes(lib));

        Err(Error::Io("execute requested command", command.exec()))
    }

    fn provision(config: &Config<'_>, venv: &Path) -> Result<(), Error> {
        let python = if config.python == PYTHON_VERSION {
            Cow::Borrowed(PYTHON_REQUEST)
        } else {
            // Each process argument must be contiguous, including a configured version.
            Cow::Owned(format!("cpython@{}", config.python))
        };
        let spacy = if config.spacy == SPACY_VERSION {
            Cow::Borrowed(SPACY_REQUIREMENT)
        } else {
            Cow::Owned(format!("spacy=={}", config.spacy))
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .map_err(|e| Error::Io("initialize Tokio runtime", e))?;
        let ret = runtime.block_on(async {
            let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .map_err(|e| Error::Io("listen for SIGINT", e))?;
            tokio::select! {
                ret = async {
                    eprintln!("Checking uv executable {:?}", config.uv);
                    run_uv(config.uv, [OsStr::new("--version")]).await?;

                    eprintln!("Preparing {} with CPython {}", venv.display(), config.python);
                    run_uv(config.uv, [
                        OsStr::new("venv"), OsStr::new("--no-project"), OsStr::new("--allow-existing"),
                        OsStr::new("--python"), OsStr::new(python.as_ref()), venv.as_os_str(),
                    ]).await?;

                    eprintln!("Installing spaCy {} and pip", config.spacy);
                    run_uv(config.uv, [
                        OsStr::new("pip"), OsStr::new("install"), OsStr::new("--python"),
                        venv.as_os_str(), OsStr::new("pip"), OsStr::new(spacy.as_ref()),
                    ]).await
                } => ret,
                _ = interrupt.recv() => Err(Error::Interrupted),
            }
        });
        drop(runtime);
        if ret.is_ok() {
            // Dropping Tokio's listener does not restore the synchronous SIGINT behavior.
            reset_signal(tokio::signal::unix::SignalKind::interrupt().as_raw_value())
                .map_err(|e| Error::Io("restore SIGINT disposition", e))?;
        }
        ret
    }

    async fn run_uv(uv: &OsStr, args: impl IntoIterator<Item = &OsStr>) -> Result<(), Error> {
        let status = tokio::process::Command::new(uv)
            .args(args)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .map_err(|e| Error::Io("execute uv", e))?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Child("uv", status))
        }
    }

    fn reset_signal(sig: c_int) -> io::Result<()> {
        unsafe extern "C" {
            fn signal(
                sig: c_int,
                handler: Option<unsafe extern "C" fn(c_int)>,
            ) -> Option<unsafe extern "C" fn(c_int)>;
        }
        // Linux and macOS represent SIG_DFL as null and SIG_ERR as -1. The returned
        // function pointer is inspected only as an address; it is never called.
        let previous = unsafe { signal(sig, None) };
        if previous.is_some_and(|handler| handler as *const () as usize == usize::MAX) {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn python_metadata(python: &Path) -> Result<Vec<u8>, Error> {
        // Paths are unbounded OS strings; retain one output buffer and borrow its fields.
        let output = Command::new(python)
            .args([
                "-I",
                "-c",
                r#"
import os
import sys
import sysconfig

# Include the standard library and extensions for executables embedding Python.
modules = os.pathsep.join(sys.path)
for path in (sysconfig.get_config_var("LIBDIR"), modules):
    sys.stdout.buffer.write(os.fsencode(path))
    sys.stdout.buffer.write(b"\0")
"#,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| Error::Io("read Python paths", e))?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(Error::Child("Python path query", output.status))
        }
    }

    fn prepend_env(command: &mut Command, name: &str, prefix: &OsStr) {
        if let Some(old) = env::var_os(name).filter(|s| !s.is_empty()) {
            // A changed environment value must be passed to exec as one OS string.
            let mut value = prefix.to_os_string();
            value.push(":");
            value.push(old);
            command.env(name, value);
        } else {
            command.env(name, prefix);
        }
    }

    enum Error {
        Arguments(clap::Error),
        Matches(MatchesError),
        Missing(&'static str),
        Invalid(&'static str),
        Io(&'static str, io::Error),
        Child(&'static str, ExitStatus),
        Interrupted,
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Arguments(e) => e.fmt(f),
                Self::Matches(e) => e.fmt(f),
                Self::Missing(name) => write!(f, "missing {name}"),
                Self::Invalid(message) => f.write_str(message),
                Self::Io(operation, e) => write!(f, "failed to {operation}: {e}"),
                Self::Child(program, status) => write!(f, "{program} failed with {status}"),
                Self::Interrupted => f.write_str("interrupted"),
            }
        }
    }

    struct Outcome(Result<(), Error>);

    impl Termination for Outcome {
        fn report(self) -> ExitCode {
            match self.0 {
                Ok(()) => ExitCode::SUCCESS,
                Err(Error::Arguments(e)) => {
                    if e.use_stderr() {
                        eprint!("{e}");
                    } else {
                        print!("{e}");
                    }
                    ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(1))
                }
                Err(Error::Child(program, status)) => {
                    eprintln!("spacy-runner: {program} failed with {status}");
                    if let Some(sig) = status.signal() {
                        unsafe extern "C" {
                            fn raise(sig: c_int) -> c_int;
                        }
                        // SIGKILL cannot be reset, but must still be raised. If delivery
                        // returns (for example, a blocked signal), use the numeric fallback.
                        let _ = reset_signal(sig);
                        unsafe { raise(sig) };
                    }
                    let code = status
                        .code()
                        .or_else(|| status.signal().and_then(|n| n.checked_add(128)));
                    ExitCode::from(code.and_then(|n| u8::try_from(n).ok()).unwrap_or(1))
                }
                Err(e) => {
                    eprintln!("spacy-runner: {e}");
                    if matches!(e, Error::Interrupted) {
                        ExitCode::from(130)
                    } else {
                        ExitCode::FAILURE
                    }
                }
            }
        }
    }
}
