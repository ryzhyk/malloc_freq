use args::Args;
use getopts::Occur;
use glob::glob;
use malloc_freq::Profile;
use std::fs;

const PROGRAM_DESC: &str = "Visualize malloc_freq profiles";
const PROGRAM_NAME: &str = "mf_print";

fn main() -> Result<(), anyhow::Error> {
    let mut args = Args::new(PROGRAM_NAME, PROGRAM_DESC);
    args.flag("h", "help", "Print a help message")
        .option(
            "d",
            "dir",
            "Directory that stores target profile",
            "DIR",
            Occur::Optional,
            None,
        )
        .option(
            "t",
            "threshold",
            "Significance threshold, in percent",
            "<m.n>",
            Occur::Optional,
            Some("1.0".to_string()),
        );

    args.parse_from_cli()?;

    if args.value_of::<bool>("help").unwrap() == true {
        println!("{}", args.full_usage());
        return Ok(());
    }

    let dir: String = args.value_of("dir")?;
    let wildcard = format!("{}/malloc_freq.*", dir);

    let threshold_str: String = args.value_of("threshold").unwrap();
    let threshold: f64 = threshold_str.parse()?;
    if threshold > 100.0 {
        return Err(anyhow::Error::msg("Threshold value cannot exceed 100%"));
    }

    let mut profiles = vec![];

    for path in glob(wildcard.as_str())? {
        let path = path?;
        eprintln!("found profile in {}", path.display());
        let profile_bytes = fs::read(path)?;
        profiles.push(serde_yaml::from_slice::<Profile>(&profile_bytes[..])?);
    }

    // Aggregate per-thread profiles.
    let mut aggregate_profile = Profile::new();

    for profile in profiles.iter() {
        aggregate_profile.merge(profile);
    }

    let mut profile_string = String::new();
    aggregate_profile.fmt_with_threshold(threshold, &mut profile_string)?;
    println!("{}", profile_string);

    Ok(())
}
