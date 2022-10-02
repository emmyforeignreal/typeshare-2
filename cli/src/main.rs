//! This is the command line tool for Typeshare. It is used to generate source files in other
//! languages based on Rust code.

use clap::{command, Arg, ArgMatches};
use config::Config;
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use ignore::WalkBuilder;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::{fs, path::Path};
use typeshare_core::{
    language::{Go, Kotlin, Language, Swift, TypeScript},
    parser::ParsedData,
};

mod config;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const ARG_TYPE: &str = "TYPE";
const ARG_SWIFT_PREFIX: &str = "SWIFTPREFIX";
const ARG_JAVA_PACKAGE: &str = "JAVAPACKAGE";
const ARG_MODULE_NAME: &str = "MODULENAME";
const ARG_GO_PACKAGE: &str = "GOPACKAGE";
const ARG_CONFIG_FILE_NAME: &str = "CONFIGFILENAME";
const ARG_GENERATE_CONFIG: &str = "generate-config-file";
const ARG_OUTPUT_FILE: &str = "output-file";

fn main() {
    let options = command!()
        .version(VERSION)
        .arg(
            Arg::new(ARG_TYPE)
                .short('l')
                .long("lang")
                .help("Language of generated types")
                .takes_value(true)
                .possible_values(&["kotlin", "swift", "typescript", "go"])
                .required_unless(ARG_GENERATE_CONFIG),
        )
        .arg(
            Arg::new(ARG_SWIFT_PREFIX)
                .short('s')
                .long("swift-prefix")
                .help("Prefix for generated Swift types")
                .takes_value(true)
                .required(false),
        )
        .arg(
            Arg::new(ARG_JAVA_PACKAGE)
                .short('j')
                .long("java-package")
                .help("JAVA package name")
                .takes_value(true)
                .required(false),
        )
        .arg(
            Arg::new(ARG_MODULE_NAME)
                .short('m')
                .long("module-name")
                .help("Kotlin serializer module name")
                .takes_value(true)
                .required(false),
        )
        .arg(
            Arg::new(ARG_GO_PACKAGE)
                .long("go-package")
                .help("Go package name")
                .takes_value(true)
                .required_if(ARG_TYPE, "go"),
        )
        .arg(
            Arg::new(ARG_CONFIG_FILE_NAME)
                .short('c')
                .long("config-file")
                .help("Configuration file for typeshare")
                .takes_value(true)
                .required(false),
        )
        .arg(
            Arg::new(ARG_GENERATE_CONFIG)
                .short('g')
                .long("generate-config-file")
                .help("Generates a configuration file based on the other options specified. The file will be written to typeshare.toml by default or to the file path specified by the --config-file option.")
                .takes_value(false)
                .required(false),
        )
        .arg(
            Arg::new(ARG_OUTPUT_FILE)
                .short('o')
                .long("output-file")
                .help("File to write output to. mtime will be preserved if the file contents don't change")
                .required_unless(ARG_GENERATE_CONFIG)
                .takes_value(true)
                .long(ARG_OUTPUT_FILE)
        )
        .arg(
            Arg::new("directories")
                .help("Directories within which to recursively find and process rust files")
                .required_unless(ARG_GENERATE_CONFIG)
                .min_values(1),
        )
        .get_matches();

    let config_file = options.value_of(ARG_CONFIG_FILE_NAME);
    let config = config::load_config(config_file).unwrap_or_else(|error| {
        panic!("Unable to read configuration file due to error: {}", error);
    });
    let config = override_configuration(config, &options);

    if options.is_present(ARG_GENERATE_CONFIG) {
        let config = override_configuration(Config::default(), &options);
        let file_path = options.value_of(ARG_CONFIG_FILE_NAME);
        config::store_config(&config, file_path)
            .unwrap_or_else(|e| panic!("Failed to create new config file due to: {}", e));
        return;
    }

    let language_type = options.value_of(ARG_TYPE);
    let mut directories = options.values_of("directories").unwrap();
    let outfile = Path::new(options.value_of(ARG_OUTPUT_FILE).unwrap());

    let lang: Box<dyn Language> = match language_type {
        Some("swift") => Box::new(Swift {
            prefix: config.swift.prefix,
            type_mappings: config.swift.type_mappings,
            ..Default::default()
        }),
        Some("kotlin") => Box::new(Kotlin {
            package: config.kotlin.package,
            module_name: config.kotlin.module_name,
            type_mappings: config.kotlin.type_mappings,
        }),
        Some("typescript") => Box::new(TypeScript {
            type_mappings: config.typescript.type_mappings,
        }),
        Some("go") => Box::new(Go {
            package: config.go.package,
            type_mappings: config.go.type_mappings,
            uppercase_abbreviations: config.go.uppercase_abbreviations,
        }),
        _ => {
            panic!("argument parser didn't validate ARG_TYPE correctly");
        }
    };

    let mut types = TypesBuilder::new();
    types.add("rust", "*.rs").unwrap();
    types.select("rust");

    // This is guaranteed to always have at least one value by the clap configuration
    let first_root = directories.next().unwrap();

    let overrides = OverrideBuilder::new(first_root)
        // Don't process files inside of tools/typeshare/
        .add("!**/tools/typeshare/**")
        .expect("Failed to parse override")
        .build()
        .expect("Failed to build override");

    let mut walker_builder = WalkBuilder::new(first_root);
    walker_builder.types(types.build().expect("Failed to build types"));
    walker_builder.overrides(overrides);

    for root in directories {
        walker_builder.add(root);
    }

    // The walker ignores directories that are git-ignored. If you need
    // a git-ignored directory to be processed, add the specific directory to
    // the list of directories given to typeshare when it's invoked in the
    // makefiles
    let glob_paths: Vec<String> = walker_builder
        .build()
        .filter_map(Result::ok)
        .filter(|dir_entry| !dir_entry.path().is_dir())
        .filter_map(|dir_entry| dir_entry.path().to_str().map(String::from))
        .collect();

    let mut generated_contents = vec![];
    let parsed_data = glob_paths
        .par_iter()
        .map(|filepath| {
            let data = std::fs::read_to_string(filepath).unwrap();
            let parsed_data = typeshare_core::parser::parse(&data);
            if parsed_data.is_err() {
                panic!("{}", parsed_data.err().unwrap());
            }
            parsed_data.unwrap()
        })
        .reduce(ParsedData::default, |mut identity, other| {
            identity.add(other);
            identity
        });

    lang.generate_types(&mut generated_contents, &parsed_data)
        .expect("Couldn't generate types");

    match fs::read(outfile) {
        Ok(buf) if buf == generated_contents => {
            // ok! don't need to do anything :)
            // avoid writing the file to leave the mtime intact
            // for tools which might use it to know when to
            // rebuild.
            return;
        }
        _ => {}
    }

    let out_dir = outfile.parent().unwrap();
    // If the output directory doesn't already exist, create it.
    if !out_dir.exists() {
        fs::create_dir_all(out_dir).expect("failed to create output directory");
    }

    fs::write(outfile, generated_contents).expect("failed to write output");
}

/// Overrides any configuration values with provided arguments
fn override_configuration(mut config: Config, options: &ArgMatches) -> Config {
    if let Some(swift_prefix) = options.value_of(ARG_SWIFT_PREFIX) {
        config.swift.prefix = swift_prefix.to_string();
    }

    if let Some(java_package) = options.value_of(ARG_JAVA_PACKAGE) {
        config.kotlin.package = java_package.to_string();
    }

    if let Some(module_name) = options.value_of(ARG_MODULE_NAME) {
        config.kotlin.module_name = module_name.to_string();
    }

    if let Some(go_package) = options.value_of(ARG_GO_PACKAGE) {
        config.go.package = go_package.to_string();
    }

    config
}
