//! The command line.

use std::fmt;

use mare_hub::MAX_FRAMES;
use mare_sys::HubName;

/// What `mare help` prints.
pub const USAGE: &str = "\
mare runs a MARÉ hub on an audio device, for programs to play through.

Usage:
  mare devices               List the audio devices (opens none).
  mare run [options]         Run a hub until Ctrl+C.
  mare probe [--name NAME]   Show a running hub.
  mare help                  Show this.
  mare --version             Show the version.

Options of run:
  --name NAME       The hub's name, which programs connect to (default main):
                    1 to 10 of a-z, 0-9 and '-'.
  --device NAME     The output device, by its id or part of its name (see
                    mare devices); default the system's default output.
  --input NAME      A capture device opened beside the output, in the same
                    mode and at the same rate.
  --follow NAME     Another output device, joined to the first: it runs on
                    its own clock and is resampled to the first's. May be
                    given more than once; its channels come after the first's.
  --exclusive       Exclusive mode: the device's own format and smallest
                    period, with no system mixer, for this hub alone.
                    Default shared: other programs keep playing.
  --period FRAMES   The period wanted; the nearest the device allows is used
                    (default the smallest).
  --rate HZ         The sample rate, in exclusive mode (default the mixer's).
  --clients N       The most programs at once, 1 to 1024 (default 64).
  --seconds S       Stop after S seconds.
  --null            Run on no device: silence, timed by the system clock. For
                    trying programs without a sound card, on any system.
  --inputs N        The null device's input channels (default 2).
  --outputs N       The null device's output channels (default 2).
";

/// What to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Print the usage.
    Help,
    /// Print the version.
    Version,
    /// List the audio devices.
    Devices,
    /// Show the running hub of this name.
    Probe(HubName),
    /// Run a hub.
    Run(RunOptions),
}

/// Which device a hub runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceChoice {
    /// The system's default output.
    Default,
    /// The output whose id is this, or whose name contains it.
    Named(String),
    /// No device: silence, timed by the system clock.
    Null {
        /// Input channels.
        inputs: u32,
        /// Output channels.
        outputs: u32,
    },
}

/// How to run a hub.
#[derive(Clone, Debug, PartialEq)]
pub struct RunOptions {
    /// The hub's name.
    pub hub: HubName,
    /// The device that paces it.
    pub device: DeviceChoice,
    /// A capture device opened beside the output.
    pub input: Option<String>,
    /// Output devices joined to the first, on their own clocks.
    pub followers: Vec<String>,
    /// Exclusive mode rather than shared.
    pub exclusive: bool,
    /// The period wanted, in frames.
    pub period: Option<u32>,
    /// The sample rate wanted.
    pub rate: Option<u32>,
    /// The most clients at once.
    pub max_clients: u32,
    /// Stop after this many seconds.
    pub seconds: Option<f64>,
}

/// The null device's rate when none is given.
pub const NULL_RATE: u32 = 48_000;
/// The null device's period when none is given.
pub const NULL_PERIOD: u32 = 64;

/// A command line that makes no sense, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

fn usage(message: impl Into<String>) -> UsageError {
    UsageError(message.into())
}

/// Reads the arguments that follow the program's name.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, UsageError> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Ok(Command::Help);
    };
    let options = Options::split(args)?;
    match command.as_str() {
        "help" | "-h" | "--help" => Ok(Command::Help),
        "--version" | "-V" | "version" => Ok(Command::Version),
        "devices" => {
            options.only(&[])?;
            Ok(Command::Devices)
        }
        "probe" => {
            options.only(&["--name"])?;
            Ok(Command::Probe(options.hub()?))
        }
        "run" => run_options(&options).map(Command::Run),
        other => Err(usage(format!("no command {other:?}"))),
    }
}

fn run_options(options: &Options) -> Result<RunOptions, UsageError> {
    options.only(&[
        "--name",
        "--device",
        "--input",
        "--follow",
        "--exclusive",
        "--period",
        "--rate",
        "--clients",
        "--seconds",
        "--null",
        "--inputs",
        "--outputs",
    ])?;
    let null = options.flag("--null")?;
    let exclusive = options.flag("--exclusive")?;
    let input = options.single("--input")?;
    let followers = options.all("--follow");
    let device = match (null, options.single("--device")?) {
        (true, Some(_)) => return Err(usage("--null and --device exclude each other")),
        (true, None) => {
            if input.is_some() || !followers.is_empty() || exclusive {
                return Err(usage(
                    "--null runs on no device: --input, --follow and --exclusive do not apply",
                ));
            }
            DeviceChoice::Null {
                inputs: options.number("--inputs", 0, 64)?.unwrap_or(2),
                outputs: options.number("--outputs", 0, 64)?.unwrap_or(2),
            }
        }
        (false, named) => {
            if options.single("--inputs")?.is_some() || options.single("--outputs")?.is_some() {
                return Err(usage(
                    "--inputs and --outputs are the null device's; a real one has its own",
                ));
            }
            named.map_or(DeviceChoice::Default, DeviceChoice::Named)
        }
    };
    let rate = options.number("--rate", 8_000, 768_000)?;
    if rate.is_some() && !exclusive && !null {
        return Err(usage(
            "--rate needs --exclusive: shared mode runs at the system mixer's rate",
        ));
    }
    let seconds = match options.single("--seconds")? {
        None => None,
        Some(text) => match text.parse::<f64>() {
            Ok(s) if s.is_finite() && s > 0.0 => Some(s),
            _ => {
                return Err(usage(format!(
                    "--seconds wants a positive number, not {text:?}"
                )));
            }
        },
    };
    Ok(RunOptions {
        hub: options.hub()?,
        device,
        input,
        followers,
        exclusive,
        period: options.number("--period", 1, MAX_FRAMES)?,
        rate,
        max_clients: options.number("--clients", 1, 1024)?.unwrap_or(64),
        seconds,
    })
}

/// The options after a command, as `(name, value)` in order.
struct Options(Vec<(String, Option<String>)>);

/// The options that take no value.
const FLAGS: [&str; 2] = ["--exclusive", "--null"];

impl Options {
    fn split(args: impl Iterator<Item = String>) -> Result<Self, UsageError> {
        let mut args = args.peekable();
        let mut options = Vec::new();
        while let Some(arg) = args.next() {
            if !arg.starts_with("--") {
                return Err(usage(format!("unexpected {arg:?}")));
            }
            if let Some((name, value)) = arg.split_once('=') {
                options.push((name.to_owned(), Some(value.to_owned())));
            } else if FLAGS.contains(&arg.as_str()) {
                options.push((arg, None));
            } else {
                let value = args.next_if(|next| !next.starts_with("--"));
                if value.is_none() {
                    return Err(usage(format!("{arg} wants a value")));
                }
                options.push((arg, value));
            }
        }
        Ok(Self(options))
    }

    /// Refuses any option not in `known`.
    fn only(&self, known: &[&str]) -> Result<(), UsageError> {
        match self
            .0
            .iter()
            .find(|(name, _)| !known.contains(&name.as_str()))
        {
            Some((name, _)) => Err(usage(format!("no option {name} here"))),
            None => Ok(()),
        }
    }

    fn flag(&self, name: &str) -> Result<bool, UsageError> {
        match self.0.iter().find(|(n, _)| n == name) {
            None => Ok(false),
            Some((_, None)) => Ok(true),
            Some((_, Some(_))) => Err(usage(format!("{name} takes no value"))),
        }
    }

    fn single(&self, name: &str) -> Result<Option<String>, UsageError> {
        let mut found = self.0.iter().filter(|(n, _)| n == name);
        let first = found.next().and_then(|(_, value)| value.clone());
        if found.next().is_some() {
            return Err(usage(format!("{name} given twice")));
        }
        Ok(first)
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(n, _)| n == name)
            .filter_map(|(_, value)| value.clone())
            .collect()
    }

    fn number(&self, name: &str, min: u32, max: u32) -> Result<Option<u32>, UsageError> {
        let Some(text) = self.single(name)? else {
            return Ok(None);
        };
        match text.parse::<u32>() {
            Ok(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(usage(format!(
                "{name} wants a whole number from {min} to {max}, not {text:?}"
            ))),
        }
    }

    fn hub(&self) -> Result<HubName, UsageError> {
        match self.single("--name")? {
            None => Ok(HubName::default_hub()),
            Some(name) => HubName::new(&name).map_err(|e| usage(format!("--name {name:?}: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_line(line: &str) -> Result<Command, UsageError> {
        parse(line.split_whitespace().map(str::to_owned))
    }

    fn run(line: &str) -> RunOptions {
        match parse_line(line) {
            Ok(Command::Run(options)) => options,
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn a_bare_run_is_the_main_hub_on_the_default_output_shared() {
        let options = run("run");
        assert_eq!(options.hub, HubName::default_hub());
        assert_eq!(options.device, DeviceChoice::Default);
        assert!(!options.exclusive);
        assert_eq!(options.max_clients, 64);
        assert_eq!(
            (options.period, options.rate, options.seconds),
            (None, None, None)
        );
    }

    #[test]
    fn options_take_values_after_a_space_or_an_equals_sign() {
        let options = run(
            "run --name organ --device=UMC --input UMC --exclusive --period 48 --rate 96000 \
             --follow Realtek --follow=USB --clients 8 --seconds 2.5",
        );
        assert_eq!(options.hub.as_str(), "organ");
        assert_eq!(options.device, DeviceChoice::Named("UMC".into()));
        assert_eq!(options.input.as_deref(), Some("UMC"));
        assert_eq!(options.followers, ["Realtek", "USB"]);
        assert!(options.exclusive);
        assert_eq!(options.period, Some(48));
        assert_eq!(options.rate, Some(96_000));
        assert_eq!(options.max_clients, 8);
        assert_eq!(options.seconds, Some(2.5));
    }

    #[test]
    fn the_null_device_has_two_channels_each_way_unless_told() {
        assert_eq!(
            run("run --null").device,
            DeviceChoice::Null {
                inputs: 2,
                outputs: 2
            }
        );
        assert_eq!(
            run("run --null --inputs 0 --outputs 8 --rate 44100").device,
            DeviceChoice::Null {
                inputs: 0,
                outputs: 8
            }
        );
    }

    #[test]
    fn the_other_commands() {
        assert_eq!(parse_line(""), Ok(Command::Help));
        assert_eq!(parse_line("--help"), Ok(Command::Help));
        assert_eq!(parse_line("--version"), Ok(Command::Version));
        assert_eq!(parse_line("devices"), Ok(Command::Devices));
        assert_eq!(
            parse_line("probe"),
            Ok(Command::Probe(HubName::default_hub()))
        );
        assert_eq!(
            parse_line("probe --name organ"),
            Ok(Command::Probe(HubName::new("organ").unwrap()))
        );
    }

    #[test]
    fn nonsense_is_refused_with_a_reason() {
        for line in [
            "play",
            "devices --name x",
            "probe organ",
            "run --name Organ",
            "run --name",
            "run --period 0",
            "run --period many",
            "run --clients 2000",
            "run --rate 96000",
            "run --seconds -1",
            "run --exclusive=yes",
            "run --device a --device b",
            "run --null --device UMC",
            "run --null --exclusive",
            "run --null --follow USB",
            "run --inputs 2",
            "run --loud",
        ] {
            let error = parse_line(line).expect_err(line);
            assert!(!error.0.is_empty(), "{line}");
        }
    }
}
