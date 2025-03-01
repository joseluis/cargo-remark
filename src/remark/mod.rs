use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Context;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use regex::Regex;
use serde::Deserialize;
use serde_yaml::Value;

use crate::remark::parse::{MissedRemark, RemarkArg, RemarkArgCallee, RemarkArgCaller};
use crate::utils::callback::LoadCallback;
use crate::utils::timing::time_block_log_debug;
use crate::RustcSourceRoot;

mod parse;

/// We expect that the remark YAML files will have this extension.
const EXPECTED_EXTENSION: &str = ".opt.yaml";

pub type Line = u32;
pub type Column = u32;

#[derive(Debug, PartialEq)]
pub struct Location {
    pub file: String,
    pub line: Line,
    pub column: Column,
}

#[derive(Debug)]
pub struct Function {
    pub name: String,
    pub location: Option<Location>,
}

#[derive(Debug)]
pub enum MessagePart {
    String(String),
    AnnotatedString { message: String, location: Location },
}

#[derive(Debug)]
pub struct Remark {
    pub pass: String,
    pub name: String,
    pub function: Function,
    pub message: Vec<MessagePart>,
    pub hotness: Option<i32>,
}

#[derive(Default)]
pub struct RemarkLoadOptions {
    /// Load remarks from external crates
    pub external: bool,
    /// Source directory
    pub source_dir: PathBuf,
    /// Remark kinds that should be ignored
    pub filter_kind: Vec<String>,
    /// Root path of rustc toolchain sources
    pub rustc_source_root: Option<RustcSourceRoot>,
}

pub fn load_remarks_from_file<P: AsRef<Path>>(
    path: P,
    options: &RemarkLoadOptions,
) -> anyhow::Result<Vec<Remark>> {
    let path = path.as_ref();

    let file =
        File::open(path).with_context(|| format!("Cannot open remark file {}", path.display()))?;
    log::debug!("Parsing {}", path.display());

    if file.metadata()?.len() == 0 {
        log::debug!("File is empty");
        return Ok(vec![]);
    }

    let reader = BufReader::new(file);

    let remarks = time_block_log_debug("Parsed remark file", || parse_remarks(reader, options));
    Ok(remarks)
}

fn parse_remarks<R: std::io::Read>(reader: R, options: &RemarkLoadOptions) -> Vec<Remark> {
    let mut remarks = vec![];
    for document in serde_yaml::Deserializer::from_reader(reader) {
        match parse::Remark::deserialize(document) {
            Ok(remark) => {
                // TODO: optimize (intern)
                match remark {
                    parse::Remark::Missed(remark) => {
                        let MissedRemark {
                            pass,
                            name,
                            debug_loc,
                            function,
                            args,
                            hotness,
                        } = remark;

                        if let Some(location) = debug_loc {
                            if !options.external {
                                if location.file.starts_with('/') {
                                    continue;
                                }
                                if !options.source_dir.join(location.file.as_ref()).is_file() {
                                    continue;
                                }
                            }
                            if options
                                .filter_kind
                                .iter()
                                .any(|filter| filter == name.as_ref())
                            {
                                continue;
                            }

                            let remark = Remark {
                                pass: pass.to_string(),
                                name: name.to_string(),
                                function: Function {
                                    name: demangle(&function),
                                    location: Some(parse_debug_loc(options, location)),
                                },
                                message: construct_message(options, args),
                                hotness,
                            };
                            remarks.push(remark);
                        }
                    }
                    parse::Remark::Passed {} | parse::Remark::Analysis {} => {}
                }
            }
            Err(error) => {
                log::debug!("Error while deserializing remark: {error:?}");
            }
        }
    }
    remarks
}

fn construct_message(opts: &RemarkLoadOptions, arguments: Vec<RemarkArg>) -> Vec<MessagePart> {
    let mut parts = vec![];
    let mut buffer = String::new();

    let add_annotated = |part: MessagePart, buffer: &mut String, message: &mut Vec<MessagePart>| {
        if !buffer.is_empty() {
            message.push(MessagePart::String(std::mem::take(buffer)));
        }
        message.push(part);
    };
    let aggregate_keys = |buffer: &mut String, map: BTreeMap<Cow<'_, str>, Value>| {
        buffer.extend(map.into_values().filter_map(|v| match v {
            Value::Bool(value) => Some(value.to_string()),
            Value::Number(value) => Some(value.to_string()),
            Value::String(value) => Some(value),
            _ => None,
        }));
    };

    for arg in arguments {
        match arg {
            RemarkArg::String(inner) => buffer.push_str(&inner.string),
            RemarkArg::Callee(RemarkArgCallee {
                callee: function,
                debug_loc: Some(location),
            })
            | RemarkArg::Caller(RemarkArgCaller {
                caller: function,
                debug_loc: Some(location),
            }) => add_annotated(
                MessagePart::AnnotatedString {
                    message: demangle(&function),
                    location: parse_debug_loc(opts, location),
                },
                &mut buffer,
                &mut parts,
            ),
            RemarkArg::Callee(RemarkArgCallee {
                callee: function,
                debug_loc: None,
            })
            | RemarkArg::Caller(RemarkArgCaller {
                caller: function,
                debug_loc: None,
            }) => buffer.push_str(&demangle(&function)),
            RemarkArg::Reason(inner) => buffer.push_str(&inner.reason),
            RemarkArg::Other(mut inner) => {
                if let Some(location) = inner
                    .remove("DebugLoc")
                    .and_then(|l| parse::DebugLocation::deserialize(l).ok())
                {
                    let location = parse_debug_loc(opts, location);
                    let mut message = String::new();
                    aggregate_keys(&mut message, inner);
                    add_annotated(
                        MessagePart::AnnotatedString { message, location },
                        &mut buffer,
                        &mut parts,
                    );
                } else {
                    aggregate_keys(&mut buffer, inner);
                }
            }
        };
    }

    if !buffer.is_empty() {
        parts.push(MessagePart::String(buffer));
    }

    parts
}

pub fn load_remarks_from_dir<P: AsRef<Path>>(
    path: P,
    options: RemarkLoadOptions,
    callback: Option<&(dyn LoadCallback + Send + Sync)>,
) -> anyhow::Result<Vec<Remark>> {
    let dir = path
        .as_ref()
        .to_path_buf()
        .canonicalize()
        .with_context(|| format!("Cannot find remark directory {}", path.as_ref().display()))?;
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("Cannot read remark directory {}", dir.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            if !entry
                .file_name()
                .to_str()
                .map(|extension| extension.ends_with(EXPECTED_EXTENSION))
                .unwrap_or(false)
            {
                return None;
            }
            Some(entry.path())
        })
        .collect();

    log::debug!("Parsing {} file(s) from {}", files.len(), dir.display());

    if let Some(callback) = callback {
        callback.start(files.len() as u64);
    }

    let remarks: Vec<(PathBuf, anyhow::Result<Vec<Remark>>)> = files
        .into_par_iter()
        .map(|file| {
            let remarks = load_remarks_from_file(&file, &options);
            if let Some(callback) = callback {
                callback.advance();
            }
            (file, remarks)
        })
        .collect();

    let remarks = remarks
        .into_iter()
        .filter_map(|(path, result)| match result {
            Ok(remarks) => Some(remarks),
            Err(error) => {
                log::error!("Failed to load remarks from: {}: {error:?}", path.display());
                None
            }
        })
        .flatten()
        .collect();

    if let Some(callback) = callback {
        callback.finish();
    }

    Ok(remarks)
}

fn parse_debug_loc(options: &RemarkLoadOptions, location: parse::DebugLocation) -> Location {
    let file = normalize_path(options, location.file);

    Location {
        file,
        line: location.line,
        column: location.column,
    }
}

fn normalize_path(options: &RemarkLoadOptions, path: Cow<str>) -> String {
    const RUSTC_PREFIX: &str = "/rustc/";

    if let Some(ref rustc_source_root) = options.rustc_source_root {
        if let Some(path) = path.strip_prefix(RUSTC_PREFIX) {
            if let Some(index) = path.find('/') {
                let src_path = &path[index + 1..];
                let src_path = rustc_source_root.0.join(src_path);
                return src_path.to_str().unwrap().to_string().replace('\\', "/");
            }
        }
    }
    path.into_owned()
}

static HASH_REGEX: OnceLock<Regex> = OnceLock::new();

fn demangle(function: &str) -> String {
    // Remove hash from the end of legacy dmangled named
    let regex = HASH_REGEX.get_or_init(|| {
        Regex::new(r#".*::[a-z0-9]{17}$"#).expect("Could not create regular expression")
    });
    let mut demangled = rustc_demangle::demangle(function).to_string();
    if regex.find(&demangled).is_some() {
        demangled.drain(demangled.len() - 19..);
    }
    demangled
}

#[cfg(test)]
mod tests {
    use crate::remark::{parse_remarks, Remark, RemarkLoadOptions};
    use crate::RustcSourceRoot;
    use std::path::PathBuf;

    struct Options {
        external: bool,
        filter_kind: Vec<String>,
        source_dir: PathBuf,
        rustc_source_root: Option<PathBuf>,
    }

    impl Options {
        fn filter(mut self, kind: &str) -> Self {
            self.filter_kind.push(kind.to_string());
            self
        }

        fn rustc_source_root(mut self, path: &str) -> Self {
            self.rustc_source_root = Some(PathBuf::from(path));
            self
        }

        fn external(mut self, external: bool) -> Self {
            self.external = external;
            self
        }
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                external: true,
                filter_kind: vec![],
                source_dir: PathBuf::from("/tmp"),
                rustc_source_root: None,
            }
        }
    }

    impl From<Options> for RemarkLoadOptions {
        fn from(value: Options) -> Self {
            let Options {
                external,
                filter_kind,
                source_dir,
                rustc_source_root,
            } = value;
            Self {
                external,
                source_dir,
                filter_kind,
                rustc_source_root: rustc_source_root.map(RustcSourceRoot),
            }
        }
    }

    #[test]
    fn parse_single() {
        let input = r#"--- !Missed
Pass:            sdagisel
Name:            FastISelFailure
Function:        __rust_alloc
DebugLoc:        { File: '/std/src/sys_common/backtrace.rs', 
                   Line: 131, Column: 0 }
Args:
  - String:          FastISel missed call
  - String:          ': '
  - String:          '  %3 = tail call ptr @__rdl_alloc(i64 %0, i64 %1)'
  - String:          ' (in function: __rust_alloc)'
..."#;
        insta::assert_debug_snapshot!(parse(input, Options::default()), @r###"
        [
            Remark {
                pass: "sdagisel",
                name: "FastISelFailure",
                function: Function {
                    name: "__rust_alloc",
                    location: Some(
                        Location {
                            file: "/std/src/sys_common/backtrace.rs",
                            line: 131,
                            column: 0,
                        },
                    ),
                },
                message: [
                    String(
                        "FastISel missed call:   %3 = tail call ptr @__rdl_alloc(i64 %0, i64 %1) (in function: __rust_alloc)",
                    ),
                ],
                hotness: None,
            },
        ]
        "###);
    }

    #[test]
    fn parse_multiple() {
        let input = r#"--- !Missed
Pass:            inline
Name:            NoDefinition
DebugLoc:        { File: '/foo/rust/rust/library/std/src/rt.rs', 
                   Line: 165, Column: 17 }
Function:        _ZN3std2rt10lang_start17h9096f6f84fb08eb2E
Args:
  - Callee:          _ZN3std2rt19lang_start_internal17had90330d479f72f8E
  - String:          ' will not be inlined into '
  - Caller:          _ZN3std2rt10lang_start17h9096f6f84fb08eb2E
    DebugLoc:        { File: '/foo/rust/rust/library/std/src/rt.rs', 
                       Line: 159, Column: 0 }
  - String:          ' because its definition is unavailable'
...
--- !Missed
Pass:            inline
Name:            NoDefinition
DebugLoc:        { File: 'src/main.rs', Line: 7, Column: 5 }
Function:        _ZN7remarks4main17hc92ae132ef1efa8eE
Args:
  - Callee:          _ZN3std2io5stdio6_print17hdb04fec352560b87E
  - String:          ' will not be inlined into '
  - Caller:          _ZN7remarks4main17hc92ae132ef1efa8eE
    DebugLoc:        { File: 'src/main.rs', Line: 6, Column: 0 }
  - String:          ' because its definition is unavailable'
..."#;
        insta::assert_debug_snapshot!(parse(input, Options::default()), @r###"
        [
            Remark {
                pass: "inline",
                name: "NoDefinition",
                function: Function {
                    name: "std::rt::lang_start",
                    location: Some(
                        Location {
                            file: "/foo/rust/rust/library/std/src/rt.rs",
                            line: 165,
                            column: 17,
                        },
                    ),
                },
                message: [
                    String(
                        "std::rt::lang_start_internal will not be inlined into ",
                    ),
                    AnnotatedString {
                        message: "std::rt::lang_start",
                        location: Location {
                            file: "/foo/rust/rust/library/std/src/rt.rs",
                            line: 159,
                            column: 0,
                        },
                    },
                    String(
                        " because its definition is unavailable",
                    ),
                ],
                hotness: None,
            },
            Remark {
                pass: "inline",
                name: "NoDefinition",
                function: Function {
                    name: "remarks::main",
                    location: Some(
                        Location {
                            file: "src/main.rs",
                            line: 7,
                            column: 5,
                        },
                    ),
                },
                message: [
                    String(
                        "std::io::stdio::_print will not be inlined into ",
                    ),
                    AnnotatedString {
                        message: "remarks::main",
                        location: Location {
                            file: "src/main.rs",
                            line: 6,
                            column: 0,
                        },
                    },
                    String(
                        " because its definition is unavailable",
                    ),
                ],
                hotness: None,
            },
        ]
        "###);
    }

    #[test]
    fn parse_no_location() {
        let input = r#"--- !Missed
Pass:            sdagisel
Name:            FastISelFailure
Function:        __rust_alloc
Args:
  - String:          FastISel missed call
  - String:          ': '
  - String:          '  %3 = tail call ptr @__rdl_alloc(i64 %0, i64 %1)'
  - String:          ' (in function: __rust_alloc)'
..."#;
        insta::assert_debug_snapshot!(parse(input, Options::default()), @"[]");
    }

    #[test]
    fn parse_ignored_type() {
        let input = r#"--- !Passed
Pass:            inline
Name:            Inlined
DebugLoc:        { File: '/projects/personal/rust/rust/library/std/src/sys_common/backtrace.rs', 
                   Line: 135, Column: 18 }
Function:        _ZN3std10sys_common9backtrace28__rust_begin_short_backtrace17h7208ef7aa68440d8E
Args:
  - String:          ''''
  - Callee:          _ZN4core3ops8function6FnOnce9call_once17hde3380935eb1addfE
  - String:          ''' inlined into '''
  - Caller:          _ZN3std10sys_common9backtrace28__rust_begin_short_backtrace17h7208ef7aa68440d8E
    DebugLoc:        { File: '/projects/personal/rust/rust/library/std/src/sys_common/backtrace.rs', 
                       Line: 131, Column: 0 }
  - String:          ''''
  - String:          ' with '
  - String:          '(cost='
  - Cost:            '-15030'
  - String:          ', threshold='
  - Threshold:       '487'
  - String:          ')'
  - String:          ' at callsite '
  - String:          _ZN3std10sys_common9backtrace28__rust_begin_short_backtrace17h7208ef7aa68440d8E
  - String:          ':'
  - Line:            '4'
  - String:          ':'
  - Column:          '18'
  - String:          ';'
...
--- !Analysis
Pass:            size-info
Name:            FunctionMISizeChange
Function:        __rust_alloc
Args:
  - Pass:            Fast Register Allocator
  - String:          ': Function: '
  - Function:        __rust_alloc
  - String:          ': '
  - String:          'MI Instruction count changed from '
  - MIInstrsBefore:  '7'
  - String:          ' to '
  - MIInstrsAfter:   '1'
  - String:          '; Delta: '
  - Delta:           '-6'
..."#;
        assert!(parse(input, Options::default()).is_empty());
    }

    #[test]
    fn parse_gvn() {
        let input = r#"--- !Missed
Pass:            gvn
Name:            LoadClobbered
DebugLoc:        { File: '/projects/personal/rust/rust/library/core/src/result.rs', 
                   Line: 1948, Column: 15 }
Function:        '_ZN5alloc7raw_vec19RawVec$LT$T$C$A$GT$14grow_amortized17ha53db71e3f649c60E'
Args:
  - String:          'load of type '
  - Type:            i64
  - String:          ' not eliminated'
  - String:          ' because it is clobbered by '
  - ClobberedBy:     call
    DebugLoc:        { File: '/projects/personal/rust/rust/library/alloc/src/raw_vec.rs', 
                       Line: 404, Column: 19 }
..."#;

        assert_eq!(parse(input, Options::default()).len(), 1);
    }

    #[test]
    fn parse_filter() {
        let input = r#"--- !Missed
Pass:            gvn
Name:            Foo
DebugLoc:        { File: '/projects/personal/rust/rust/library/core/src/result.rs', 
                   Line: 1948, Column: 15 }
Function:        '_ZN5alloc7raw_vec19RawVec$LT$T$C$A$GT$14grow_amortized17ha53db71e3f649c60E'
Args:
  - String:          'load of type '
  - Type:            i64
  - String:          ' not eliminated'
  - String:          ' because it is clobbered by '
  - ClobberedBy:     call
    DebugLoc:        { File: '/projects/personal/rust/rust/library/alloc/src/raw_vec.rs', 
                       Line: 404, Column: 19 }
..."#;

        assert!(parse(input, Options::default().filter("Foo")).is_empty());
    }

    #[test]
    fn parse_hotness() {
        let input = r#"--- !Missed
Pass:            regalloc
Name:            LoopSpillReloadCopies
DebugLoc:        { File: '/rustc/08d00b40aef2017fe6dba3ff7d6476efa0c10888/library/std/src/io/buffered/bufreader/buffer.rs', 
                   Line: 114, Column: 13 }
Function:        _ZN3std2io16append_to_string17hcf3f6e91099a64a2E
Hotness:         2
Args:
  - NumReloads:      '3'
  - String:          ' reloads '
  - TotalReloadsCost: '4.607052e-10'
  - String:          ' total reloads cost '
  - NumVRCopies:     '2'
  - String:          ' virtual registers copies '
  - TotalCopiesCost: '5.000000e-01'
  - String:          ' total copies cost '
  - String:          generated in loop
..."#;

        insta::assert_debug_snapshot!(parse(input, Options::default()), @r###"
        [
            Remark {
                pass: "regalloc",
                name: "LoopSpillReloadCopies",
                function: Function {
                    name: "std::io::append_to_string",
                    location: Some(
                        Location {
                            file: "/rustc/08d00b40aef2017fe6dba3ff7d6476efa0c10888/library/std/src/io/buffered/bufreader/buffer.rs",
                            line: 114,
                            column: 13,
                        },
                    ),
                },
                message: [
                    String(
                        "3 reloads 4.607052e-10 total reloads cost 2 virtual registers copies 5.000000e-01 total copies cost generated in loop",
                    ),
                ],
                hotness: Some(
                    2,
                ),
            },
        ]
        "###);
    }

    #[test]
    fn parse_remap_rust_source() {
        let input = r#"--- !Missed
Pass:            regalloc
Name:            LoopSpillReloadCopies
DebugLoc:        { File: '/rustc/08d00b40aef2017fe6dba3ff7d6476efa0c10888/library/std/src/io/buffered/bufreader/buffer.rs', 
                   Line: 114, Column: 13 }
Function:        _ZN3std2io16append_to_string17hcf3f6e91099a64a2E
Args:
..."#;

        insta::assert_debug_snapshot!(parse(input, Options::default().external(true).rustc_source_root("/foo/bar")), @r###"
        [
            Remark {
                pass: "regalloc",
                name: "LoopSpillReloadCopies",
                function: Function {
                    name: "std::io::append_to_string",
                    location: Some(
                        Location {
                            file: "/foo/bar/library/std/src/io/buffered/bufreader/buffer.rs",
                            line: 114,
                            column: 13,
                        },
                    ),
                },
                message: [],
                hotness: None,
            },
        ]
        "###);
    }

    fn parse(input: &str, opts: Options) -> Vec<Remark> {
        parse_remarks(input.as_bytes(), &opts.into())
    }
}
