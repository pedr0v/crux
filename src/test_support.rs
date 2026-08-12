use crate::index::index_path;
use protobuf::{EnumOrUnknown, Message, MessageField};
use scip::types::{
    occurrence, symbol_information, Document, Index, Occurrence, Signature, SingleLineRange,
    SymbolInformation, SymbolRole,
};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const DATE_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 date.ts/formatDate().";
pub(crate) const LONG_DATE_SYMBOL: &str =
    "scip-typescript npm fixture 1.0.0 date.ts/formatDateLong().";
pub(crate) const RNG_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 rng.ts/rngState().";
pub(crate) const RNG_PROPERTY_SYMBOL: &str =
    "scip-typescript npm fixture 1.0.0 persistence.ts/SaveData#rngState.";
const USER_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 user.ts/User#";
const DEAD_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/neverUsed().";
const FILE_LOCAL_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/localOnly().";
const PARAMETER_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/localOnly().(input)";
const DEFAULT_EXPORT_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/defaultHelper().";
const INTERFACE_MEMBER_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/Worker#run().";
const EXPORTED_LIMIT_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/LIMIT.";
const PRIVATE_HELPER_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/helper.";
pub(crate) const INNER_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 date.ts/inner().";
pub(crate) const BARREL_ALIAS_SYMBOL: &str =
    "scip-typescript npm fixture 1.0.0 barrel.ts/formatDate().";
pub(crate) const DUPLICATE_DATE_SYMBOL: &str =
    "scip-typescript npm fixture 1.0.0 date.ts/formatDateDuplicate().";
pub(crate) const OBJECT_METHOD_SYMBOL: &str =
    "scip-typescript npm fixture 1.0.0 app.ts/registerCodeFix().getCodeActions.";
static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TestProject {
    pub(crate) root: PathBuf,
}

impl TestProject {
    pub(crate) fn new() -> Self {
        let sequence = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after unix epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "crux-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(root.join(".scip-nav")).expect("create test project");
        Self { root }
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(crate) fn symbol_information(
    symbol: &str,
    display_name: &str,
    kind: symbol_information::Kind,
    signature: &str,
    documentation: &str,
) -> SymbolInformation {
    SymbolInformation {
        symbol: symbol.to_string(),
        documentation: vec![documentation.to_string()],
        kind: EnumOrUnknown::new(kind),
        display_name: display_name.to_string(),
        signature_documentation: MessageField::some(Signature {
            language: "typescript".to_string(),
            text: signature.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(crate) fn legacy_occurrence(symbol: &str, line: i32, definition: bool) -> Occurrence {
    Occurrence {
        range: vec![line, 0, 8],
        symbol: symbol.to_string(),
        symbol_roles: if definition {
            SymbolRole::Definition as i32
        } else {
            SymbolRole::ReadAccess as i32
        },
        enclosing_range: if definition {
            vec![line, 0, line + 8, 0]
        } else {
            Vec::new()
        },
        ..Default::default()
    }
}

fn typed_occurrence(symbol: &str, line: i32, definition: bool) -> Occurrence {
    Occurrence {
        symbol: symbol.to_string(),
        symbol_roles: if definition {
            SymbolRole::Definition as i32
        } else {
            SymbolRole::ReadAccess as i32
        },
        typed_range: Some(occurrence::Typed_range::SingleLineRange(SingleLineRange {
            line,
            start_character: 0,
            end_character: 8,
            ..Default::default()
        })),
        enclosing_range: if definition {
            vec![line, 0, line + 8, 0]
        } else {
            Vec::new()
        },
        ..Default::default()
    }
}

pub(crate) fn fixture(include_extra_document: bool) -> Index {
    let date = symbol_information(
        DATE_SYMBOL,
        "formatDate",
        symbol_information::Kind::Function,
        "function formatDate(input: Date): string",
        "Formats a date\nfor display.",
    );
    let date_long = symbol_information(
        LONG_DATE_SYMBOL,
        "formatDateLong",
        symbol_information::Kind::Function,
        "function formatDateLong(input: Date): string",
        "Formats a long date.",
    );
    let user = symbol_information(
        USER_SYMBOL,
        "User",
        symbol_information::Kind::Class,
        "class User",
        "Application user.",
    );
    let rng = symbol_information(
        RNG_SYMBOL,
        "rngState",
        symbol_information::Kind::Function,
        "function rngState(): number",
        "Returns the next random state.",
    );
    let rng_property = symbol_information(
        RNG_PROPERTY_SYMBOL,
        "rngState",
        symbol_information::Kind::Property,
        "rngState: number",
        "Persisted random state.",
    );
    let dead = symbol_information(
        DEAD_SYMBOL,
        "neverUsed",
        symbol_information::Kind::Function,
        "function neverUsed(): void",
        "Never referenced.",
    );
    let file_local = symbol_information(
        FILE_LOCAL_SYMBOL,
        "localOnly",
        symbol_information::Kind::Function,
        "function localOnly(): void",
        "Only referenced in this file.",
    );
    let parameter = symbol_information(
        PARAMETER_SYMBOL,
        "input",
        symbol_information::Kind::Parameter,
        "input: string",
        "Function parameter.",
    );
    let default_export = symbol_information(
        DEFAULT_EXPORT_SYMBOL,
        "defaultHelper",
        symbol_information::Kind::Function,
        "function defaultHelper(): void",
        "Named default export.",
    );
    let interface_member = symbol_information(
        INTERFACE_MEMBER_SYMBOL,
        "run",
        symbol_information::Kind::Method,
        "run(): void",
        "Interface method.",
    );
    let exported_limit = symbol_information(
        EXPORTED_LIMIT_SYMBOL,
        "LIMIT",
        symbol_information::Kind::Constant,
        "const LIMIT: number",
        "Exported limit.",
    );
    let private_helper = symbol_information(
        PRIVATE_HELPER_SYMBOL,
        "helper",
        symbol_information::Kind::Constant,
        "const helper: number",
        "Private helper.",
    );

    let mut documents = vec![
        Document {
            language: "typescript".to_string(),
            relative_path: "src/lib/date.ts".to_string(),
            occurrences: vec![
                legacy_occurrence(LONG_DATE_SYMBOL, 9, true),
                legacy_occurrence(DATE_SYMBOL, 14, false),
                legacy_occurrence(DATE_SYMBOL, 41, true),
            ],
            symbols: vec![date, date_long],
            ..Default::default()
        },
        Document {
            language: "typescript".to_string(),
            relative_path: "src/lib/dead.ts".to_string(),
            occurrences: vec![
                legacy_occurrence(FILE_LOCAL_SYMBOL, 1, true),
                legacy_occurrence(PARAMETER_SYMBOL, 2, true),
                legacy_occurrence(PARAMETER_SYMBOL, 3, false),
                legacy_occurrence(FILE_LOCAL_SYMBOL, 5, false),
                legacy_occurrence(DEAD_SYMBOL, 10, true),
                legacy_occurrence(DEFAULT_EXPORT_SYMBOL, 12, true),
                legacy_occurrence(INTERFACE_MEMBER_SYMBOL, 15, true),
                legacy_occurrence(EXPORTED_LIMIT_SYMBOL, 17, true),
                legacy_occurrence(PRIVATE_HELPER_SYMBOL, 18, true),
            ],
            symbols: vec![
                dead,
                file_local,
                parameter,
                default_export,
                interface_member,
                exported_limit,
                private_helper,
            ],
            ..Default::default()
        },
        Document {
            language: "typescript".to_string(),
            relative_path: "src/model/user.ts".to_string(),
            occurrences: vec![typed_occurrence(USER_SYMBOL, 4, true)],
            symbols: vec![user],
            ..Default::default()
        },
        Document {
            language: "typescript".to_string(),
            relative_path: "src/lib/rng.ts".to_string(),
            occurrences: vec![legacy_occurrence(RNG_SYMBOL, 2, true)],
            symbols: vec![rng],
            ..Default::default()
        },
        Document {
            language: "typescript".to_string(),
            relative_path: "src/persistence.ts".to_string(),
            occurrences: vec![legacy_occurrence(RNG_PROPERTY_SYMBOL, 33, true)],
            symbols: vec![rng_property],
            ..Default::default()
        },
        Document {
            language: "typescript".to_string(),
            relative_path: "src/app.ts".to_string(),
            occurrences: vec![
                legacy_occurrence(USER_SYMBOL, 5, false),
                legacy_occurrence(DATE_SYMBOL, 7, false),
                legacy_occurrence(DATE_SYMBOL, 11, false),
                legacy_occurrence(RNG_SYMBOL, 19, false),
                legacy_occurrence(RNG_PROPERTY_SYMBOL, 24, false),
                legacy_occurrence(RNG_SYMBOL, 29, false),
                typed_occurrence(DATE_SYMBOL, 39, false),
                legacy_occurrence(LONG_DATE_SYMBOL, 40, false),
            ],
            ..Default::default()
        },
    ];

    if include_extra_document {
        documents.push(Document {
            language: "typescript".to_string(),
            relative_path: "src/extra.ts".to_string(),
            occurrences: vec![legacy_occurrence(DATE_SYMBOL, 2, false)],
            ..Default::default()
        });
    }

    Index {
        documents,
        ..Default::default()
    }
}

fn write_index(project: &TestProject, index: &Index) {
    let bytes = index.write_to_bytes().expect("serialize fixture");
    fs::write(index_path(&project.root), bytes).expect("write fixture");
}

fn write_source(
    project: &TestProject,
    relative_path: &str,
    line_count: usize,
    populated_lines: &[(usize, &str)],
) {
    let path = project.root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source parent")).expect("create source parent");
    let mut lines = vec![String::new(); line_count];
    for (line, source) in populated_lines {
        lines[*line] = (*source).to_string();
    }
    fs::write(path, lines.join("\n")).expect("write source fixture");
}

pub(crate) fn write_fixture(project: &TestProject, include_extra_document: bool) {
    write_index(project, &fixture(include_extra_document));
    write_source(
        project,
        "src/lib/date.ts",
        50,
        &[
            (9, "export function formatDateLong(input: Date): string {"),
            (14, "return formatDate(input);"),
            (41, "export function formatDate(input: Date): string {"),
        ],
    );
    write_source(
        project,
        "src/lib/dead.ts",
        24,
        &[
            (1, "export function localOnly(input: string): void {"),
            (2, "void input;"),
            (3, "console.log(input);"),
            (5, "localOnly('again');"),
            (10, "export function neverUsed(): void {}"),
            (12, "export default function defaultHelper(): void {}"),
            (14, "export interface Worker {"),
            (15, "run(): void;"),
            (16, "}"),
            (17, "export const LIMIT = 5;"),
            (18, "const helper = 1;"),
        ],
    );
    write_source(
        project,
        "src/model/user.ts",
        10,
        &[(4, "export class User {")],
    );
    write_source(
        project,
        "src/lib/rng.ts",
        10,
        &[(2, "export function rngState(): number {")],
    );
    write_source(project, "src/persistence.ts", 40, &[(33, "rngState: 7,")]);
    write_source(
        project,
        "src/app.ts",
        45,
        &[
            (5, "const user = new User();"),
            (6, "import {"),
            (7, "formatDate"),
            (8, "} from './lib/date';"),
            (11, "import { formatDate } from './lib/date';"),
            (19, "const seed = rngState();"),
            (24, "save.rngState += 1;"),
            (29, "export const EPOCH = rngState();"),
            (31, "getCodeActions(context) {"),
            (34, "return formatDate(today);"),
            (36, "}"),
            (39, "const label = formatDate(today);"),
            (40, "const longLabel = formatDateLong(today);"),
        ],
    );
    if include_extra_document {
        write_source(
            project,
            "src/extra.ts",
            5,
            &[(2, "const extraLabel = formatDate(today);")],
        );
    }
}
