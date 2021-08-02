use args::Args;
use getopts::Occur;
use glob::glob;
use malloc_freq::Profile;
use std::fs;

const PROGRAM_DESC: &str = "Visualize malloc_freq profiles";
const PROGRAM_NAME: &str = "mf_print";

fn main() -> Result<(), anyhow::Error> {
    let mut args = Args::new(PROGRAM_NAME, PROGRAM_DESC);
    args.option(
        "d",
        "dir",
        "Directory that stores target profile",
        "DIR",
        Occur::Req,
        None,
    );

    args.parse_from_cli()?;

    let dir: String = args.value_of("dir").unwrap();
    let wildcard = format!("{}/malloc_freq.*", dir);

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

    eprintln!("Aggregate profile:\n{}", aggregate_profile);

    Ok(())
}
