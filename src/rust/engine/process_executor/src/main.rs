// Copyright 2017 Pants project contributors (see CONTRIBUTORS.md).
// Licensed under the Apache License, Version 2.0 (see LICENSE).

#![deny(warnings)]
// Enable all clippy lints except for many of the pedantic ones. It's a shame this needs to be copied and pasted across crates, but there doesn't appear to be a way to include inner attributes from a common source.
#![deny(
  clippy::all,
  clippy::default_trait_access,
  clippy::expl_impl_clone_on_copy,
  clippy::if_not_else,
  clippy::needless_continue,
  clippy::single_match_else,
  clippy::unseparated_literal_suffix,
  clippy::used_underscore_binding
)]
// It is often more clear to show that nothing is being moved.
#![allow(clippy::match_ref_pats)]
// Subjective style.
#![allow(
  clippy::len_without_is_empty,
  clippy::redundant_field_names,
  clippy::too_many_arguments
)]
// Default isn't as big a deal as people seem to think it is.
#![allow(clippy::new_without_default, clippy::new_ret_no_self)]
// Arc<Mutex> can be more clear than needing to grok Orderings:
#![allow(clippy::mutex_atomic)]

use clap;
use env_logger;

use process_execution;

use clap::{value_t, App, AppSettings, Arg};
use hashing::{Digest, Fingerprint};
use process_execution::{Context, ExecuteProcessRequestMetadata, Platform};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryFrom;
use std::iter::{FromIterator, Iterator};
use std::path::PathBuf;
use std::process::exit;
use std::time::Duration;
use store::{BackoffConfig, Store};
use tokio::runtime::Runtime;
use workunit_store::WorkUnitStore;

/// A binary which takes args of format:
///  process_executor --env=FOO=bar --env=SOME=value --input-digest=abc123 --input-digest-length=80
///    -- /path/to/binary --flag --otherflag
/// and runs /path/to/binary --flag --otherflag with FOO and SOME set.
/// It outputs its output/err to stdout/err, and exits with its exit code.
///
/// It does not perform $PATH lookup or shell expansion.
fn main() {
  env_logger::init();

  let args = App::new("process_executor")
    .arg(
      Arg::with_name("work-dir")
        .long("work-dir")
        .takes_value(true)
        .help("Path to workdir"),
    )
    .arg(
      Arg::with_name("local-store-path")
        .long("local-store-path")
        .takes_value(true)
        .help("Path to lmdb directory used for local file storage"),
    )
    .arg(
      Arg::with_name("input-digest")
        .long("input-digest")
        .takes_value(true)
        .required(true)
        .help("Fingerprint (hex string) of the digest to use as the input file tree."),
    )
    .arg(
      Arg::with_name("input-digest-length")
        .long("input-digest-length")
        .takes_value(true)
        .required(true)
        .help("Length of the proto-bytes whose digest to use as the input file tree."),
    )
    .arg(
      Arg::with_name("server")
        .long("server")
        .takes_value(true)
        .help(
          "The host:port of the gRPC server to connect to. Forces remote execution. \
           If unspecified, local execution will be performed.",
        ),
    )
      .arg(
        Arg::with_name("execution-root-ca-cert-file")
            .help("Path to file containing root certificate authority certificates for the execution server. If not set, TLS will not be used when connecting to the execution server.")
            .takes_value(true)
            .long("execution-root-ca-cert-file")
            .required(false)
      )
      .arg(
        Arg::with_name("execution-oauth-bearer-token-path")
            .help("Path to file containing oauth bearer token for communication with the execution server. If not set, no authorization will be provided to remote servers.")
            .takes_value(true)
            .long("execution-oauth-bearer-token-path")
            .required(false)
      )
      .arg(
      Arg::with_name("cas-server")
        .long("cas-server")
        .takes_value(true)
        .help("The host:port of the gRPC CAS server to connect to."),
    )
      .arg(
        Arg::with_name("cas-root-ca-cert-file")
            .help("Path to file containing root certificate authority certificates for the CAS server. If not set, TLS will not be used when connecting to the CAS server.")
            .takes_value(true)
            .long("cas-root-ca-cert-file")
            .required(false)
      )
      .arg(
        Arg::with_name("cas-oauth-bearer-token-path")
            .help("Path to file containing oauth bearer token for communication with the CAS server. If not set, no authorization will be provided to remote servers.")
            .takes_value(true)
            .long("cas-oauth-bearer-token-path")
            .required(false)
      )
      .arg(Arg::with_name("remote-instance-name")
          .takes_value(true)
          .long("remote-instance-name")
          .required(false))
      .arg(Arg::with_name("cache-key-gen-version")
          .takes_value(true)
          .long("cache-key-gen-version")
          .required(false))
      .arg(
        Arg::with_name("upload-chunk-bytes")
            .help("Number of bytes to include per-chunk when uploading bytes. grpc imposes a hard message-size limit of around 4MB.")
            .takes_value(true)
            .long("chunk-bytes")
            .required(false)
            .default_value("3145728") // 3MB
      )
    .arg(
      Arg::with_name("extra-platform-property")
        .long("extra-platform-property")
        .takes_value(true)
        .multiple(true)
        .help("Extra platform properties to set on the execution request."),
    )
      .arg(
        Arg::with_name("header")
            .long("header")
            .takes_value(true)
            .multiple(true)
            .help("Extra header to pass on remote execution request."),
      )
    .arg(
      Arg::with_name("env")
        .long("env")
        .takes_value(true)
        .multiple(true)
        .help("Environment variables with which the process should be run."),
    )
      .arg(
        Arg::with_name("jdk")
            .long("jdk")
            .takes_value(true)
            .required(false)
            .help("Symlink a JDK from .jdk in the working directory. For local execution, symlinks to the value of this flag. For remote execution, just requests that some JDK is symlinked if this flag has any value. https://github.com/pantsbuild/pants/issues/6416 will make this less weird in the future.")
      )
      .arg(
        Arg::with_name("target-platform")
            .long("target-platform")
            .takes_value(true)
            .required(true)
            .help("The name of the platform that this request's output is compatible with. Options are 'linux', 'darwin', or 'none' (which indicates either)")
      )
      .arg(
          Arg::with_name("use-nailgun")
              .long("use-nailgun")
              .takes_value(true)
              .required(false)
              .default_value("false")
              .help("Whether or not to enable running the process through a Nailgun server.\
                        This will likely start a new Nailgun server as a side effect.")
      )
    .setting(AppSettings::TrailingVarArg)
    .arg(
      Arg::with_name("argv")
        .multiple(true)
        .last(true)
        .required(true),
    )
    .arg(
        Arg::with_name("output-file-path")
            .long("output-file-path")
            .takes_value(true)
            .multiple(true)
            .required(false)
            .help("Path to file that is considered to be output."),
    )
    .arg(
      Arg::with_name("output-directory-path")
          .long("output-directory-path")
          .takes_value(true)
          .multiple(true)
          .required(false)
          .help("Path to directory that is considered to be output."),
    )
    .arg(
      Arg::with_name("materialize-output-to")
          .long("materialize-output-to")
          .takes_value(true)
          .required(false)
          .help("The name of a directory (which may or may not exist), where the output tree will be materialized.")
    )
    .arg(
      Arg::with_name("store-connection-limit")
          .help("Number of concurrent servers to allow connections to.")
          .takes_value(true)
          .long("store-connection-limit")
          .required(false)
          .default_value("3")
    )
    .get_matches();

  let argv: Vec<String> = args
    .values_of("argv")
    .unwrap()
    .map(str::to_string)
    .collect();
  let env = args
    .values_of("env")
    .map(collection_from_keyvalues::<_, BTreeMap<_, _>>)
    .unwrap_or_default();
  let platform_properties = args
    .values_of("extra-platform-property")
    .map(collection_from_keyvalues::<_, Vec<_>>)
    .unwrap_or_default();
  let work_dir = args
    .value_of("work-dir")
    .map(PathBuf::from)
    .unwrap_or_else(std::env::temp_dir);
  let local_store_path = args
    .value_of("local-store-path")
    .map(PathBuf::from)
    .unwrap_or_else(Store::default_path);
  let server_arg = args.value_of("server");
  let remote_instance_arg = args.value_of("remote-instance-name").map(str::to_owned);
  let output_files = if let Some(values) = args.values_of("output-file-path") {
    values.map(PathBuf::from).collect()
  } else {
    BTreeSet::new()
  };
  let output_directories = if let Some(values) = args.values_of("output-directory-path") {
    values.map(PathBuf::from).collect()
  } else {
    BTreeSet::new()
  };
  let headers = args
    .values_of("headers")
    .map(collection_from_keyvalues::<_, BTreeMap<_, _>>)
    .unwrap_or_default();

  let executor = task_executor::Executor::new();

  let store = match (server_arg, args.value_of("cas-server")) {
    (Some(_server), Some(cas_server)) => {
      let chunk_size =
        value_t!(args.value_of("upload-chunk-bytes"), usize).expect("Bad upload-chunk-bytes flag");

      let root_ca_certs = if let Some(path) = args.value_of("cas-root-ca-cert-file") {
        Some(std::fs::read(path).expect("Error reading root CA certs file"))
      } else {
        None
      };

      let oauth_bearer_token = if let Some(path) = args.value_of("cas-oauth-bearer-token-path") {
        Some(std::fs::read_to_string(path).expect("Error reading oauth bearer token file"))
      } else {
        None
      };

      Store::with_remote(
        executor.clone(),
        local_store_path,
        vec![cas_server.to_owned()],
        remote_instance_arg.clone(),
        root_ca_certs,
        oauth_bearer_token,
        1,
        chunk_size,
        Duration::from_secs(30),
        // TODO: Take a command line arg.
        BackoffConfig::new(Duration::from_secs(1), 1.2, Duration::from_secs(20)).unwrap(),
        3,
        value_t!(args.value_of("store-connection-limit"), usize)
          .expect("Bad store-connection-limit flag"),
      )
    }
    (None, None) => Store::local_only(executor.clone(), local_store_path),
    _ => panic!("Must specify either both --server and --cas-server or neither."),
  }
  .expect("Error making store");

  let input_files = {
    let fingerprint = Fingerprint::from_hex_string(args.value_of("input-digest").unwrap())
      .expect("Bad input-digest");
    let length = args
      .value_of("input-digest-length")
      .unwrap()
      .parse::<usize>()
      .expect("input-digest-length must be a non-negative number");
    Digest(fingerprint, length)
  };

  let is_nailgunnable: bool = args.value_of("use-nailgun").unwrap().parse().unwrap();

  let request = process_execution::ExecuteProcessRequest {
    argv,
    env,
    input_files,
    output_files,
    output_directories,
    timeout: Duration::new(15 * 60, 0),
    description: "process_executor".to_string(),
    unsafe_local_only_files_because_we_favor_speed_over_correctness_for_this_rule:
      hashing::EMPTY_DIGEST,
    jdk_home: args.value_of("jdk").map(PathBuf::from),
    target_platform: Platform::try_from(&args.value_of("target-platform").unwrap().to_string())
      .expect("invalid value for `target-platform"),
    is_nailgunnable,
  };

  let runner: Box<dyn process_execution::CommandRunner> = match server_arg {
    Some(address) => {
      let root_ca_certs = if let Some(path) = args.value_of("execution-root-ca-cert-file") {
        Some(std::fs::read(path).expect("Error reading root CA certs file"))
      } else {
        None
      };

      let oauth_bearer_token =
        if let Some(path) = args.value_of("execution-oauth-bearer-token-path") {
          Some(std::fs::read_to_string(path).expect("Error reading oauth bearer token file"))
        } else {
          None
        };

      Box::new(
        process_execution::remote::CommandRunner::new(
          address,
          ExecuteProcessRequestMetadata {
            instance_name: remote_instance_arg,
            cache_key_gen_version: args.value_of("cache-key-gen-version").map(str::to_owned),
            platform_properties,
          },
          root_ca_certs,
          oauth_bearer_token,
          headers,
          store.clone(),
          Platform::Linux,
          executor.clone(),
          std::time::Duration::from_secs(160),
          std::time::Duration::from_millis(500),
          std::time::Duration::from_secs(5),
        )
        .expect("Failed to make command runner"),
      ) as Box<dyn process_execution::CommandRunner>
    }
    None => Box::new(process_execution::local::CommandRunner::new(
      store.clone(),
      executor,
      work_dir,
      true,
    )) as Box<dyn process_execution::CommandRunner>,
  };

  let mut runtime = Runtime::new().unwrap();

  let result = runtime
    .block_on(runner.run(request.into(), Context::default()))
    .expect("Error executing");

  if let Some(output) = args.value_of("materialize-output-to").map(PathBuf::from) {
    runtime
      .block_on(store.materialize_directory(output, result.output_directory, WorkUnitStore::new()))
      .unwrap();
  }

  print!("{}", String::from_utf8(result.stdout.to_vec()).unwrap());
  eprint!("{}", String::from_utf8(result.stderr.to_vec()).unwrap());
  exit(result.exit_code);
}

fn collection_from_keyvalues<'a, It, Col>(keyvalues: It) -> Col
where
  It: Iterator<Item = &'a str>,
  Col: FromIterator<(String, String)>,
{
  keyvalues
    .map(|kv| {
      let mut parts = kv.splitn(2, '=');
      (
        parts.next().unwrap().to_string(),
        parts.next().unwrap_or_default().to_string(),
      )
    })
    .collect()
}
