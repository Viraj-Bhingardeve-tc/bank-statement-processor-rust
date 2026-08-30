//! bank_detection.rs — Port of `src/engines/bank-detection-engine.js`
//!
//! Detection chain (confidence, highest first):
//!   1. Labeled IFSC in header text  → 0.98
//!   2. Domain / email pattern       → 0.95
//!   3. Phrase match in header text  → 0.95
//!   4. Fuzzy abbreviation in header → 0.72–0.85
//!   5. Phrase match in full text    → capped at 0.80
//!   6. Filename                     → capped at 0.65
//!   7. Narration IFSC frequency     → 0.55

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

// ── IFSC prefix → canonical bank name ────────────────────────────────────────

static IFSC_MAP: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("HDFC", "HDFC Bank");
    m.insert("SBIN", "State Bank of India");
    m.insert("ICIC", "ICICI Bank");
    m.insert("UTIB", "Axis Bank");
    m.insert("KKBK", "Kotak Mahindra Bank");
    m.insert("PUNB", "Punjab National Bank");
    m.insert("BARB", "Bank of Baroda");
    m.insert("BKID", "Bank of India");
    m.insert("MAHB", "Bank of Maharashtra");
    m.insert("CNRB", "Canara Bank");
    m.insert("UBIN", "Union Bank of India");
    m.insert("IDFB", "IDFC First Bank");
    m.insert("IBKL", "IDBI Bank");
    m.insert("YESB", "YES Bank");
    m.insert("RATN", "RBL Bank");
    m.insert("FDRL", "Federal Bank");
    m.insert("INDB", "IndusInd Bank");
    m.insert("UCBA", "UCO Bank");
    m.insert("IOBA", "Indian Overseas Bank");
    m.insert("IDIB", "Indian Bank");
    m.insert("AUBL", "AU Small Finance Bank");
    m.insert("BDBL", "Bandhan Bank");
    m.insert("ESFB", "Equitas Small Finance Bank");
    m.insert("UJVN", "Ujjivan Small Finance Bank");
    m.insert("JSFB", "Jana Small Finance Bank");
    m.insert("DCBL", "DCB Bank");
    m.insert("KARB", "Karnataka Bank");
    m.insert("SIBL", "South Indian Bank");
    m.insert("CIUB", "City Union Bank");
    m.insert("TMBL", "Tamilnad Mercantile Bank");
    m.insert("LAVB", "Lakshmi Vilas Bank");
    m.insert("NNSB", "Nainital Bank");
    m.insert("JAKA", "J&K Bank");
    m.insert("PYTM", "Paytm Payments Bank");
    m.insert("AIRP", "Airtel Payments Bank");
    m.insert("FINO", "Fino Payments Bank");
    m.insert("IPPB", "India Post Payments Bank");
    m.insert("CBIN", "Central Bank of India");
    m.insert("PSIB", "Punjab and Sind Bank");
    m.insert("COSB", "Cosmos Co-operative Bank");
    m.insert("SRCB", "Saraswat Co-op Bank");
    // Verified against real-world IFSC listings (Razorpay/ClearTax IFSC
    // lookup, e.g. MCBL0960052, MCBL0960061) — consistent "MCBL" prefix
    // across every branch. Not exercised by the real fixture this bank was
    // added for (its own header/IFSC text is missing from the file — see
    // the Mahanagar PHRASE_MAP entry's doc comment), but registering it
    // here is what lets a *future* Mahanagar statement that does carry a
    // real header/IFSC get detected at full confidence (P1, 0.98) instead
    // of falling all the way to the filename tier.
    m.insert("MCBL", "Mahanagar Co-operative Bank");
    m.insert("SCBL", "Standard Chartered Bank");
    m.insert("HSBC", "HSBC Bank");
    m.insert("CITI", "Citibank");
    m.insert("DEUT", "Deutsche Bank");
    m.insert("BNPA", "BNP Paribas");
    m.insert("RBIS", "Reserve Bank of India");
    m
});

// ── Phrase map entry ──────────────────────────────────────────────────────────

struct PhraseEntry {
    bank: &'static str,
    weight: f64,
    phrases: &'static [&'static str],
}

// Ordered: most specific first. Same order as JS PHRASE_MAP.
static PHRASE_MAP: &[PhraseEntry] = &[
    PhraseEntry {
        bank: "HDFC Bank",
        weight: 1.0,
        phrases: &[
            "hdfc bank",
            "hdfc bank ltd",
            "hdfc bank limited",
            "hdfcbank",
            "hdfcbk",
            "hdfcbank.com",
        ],
    },
    PhraseEntry {
        bank: "State Bank of India",
        weight: 1.0,
        phrases: &[
            "state bank of india",
            "sbi bank",
            "sbi branch",
            "sbi.co.in",
            "onlinesbi.com",
            "state bank",
        ],
    },
    PhraseEntry {
        bank: "ICICI Bank",
        weight: 1.0,
        phrases: &[
            "icici bank",
            "icici bank limited",
            "icicibank",
            "icicibank.com",
        ],
    },
    PhraseEntry {
        bank: "Axis Bank",
        weight: 1.0,
        phrases: &[
            "axis bank",
            "axis bank ltd",
            "axis bank limited",
            "axisbank",
            "axisbank.com",
        ],
    },
    PhraseEntry {
        bank: "Kotak Mahindra Bank",
        weight: 1.0,
        phrases: &[
            "kotak mahindra bank",
            "kotak mahindra",
            "kotakbank.com",
            "kotak bank",
            "kotak",
        ],
    },
    PhraseEntry {
        bank: "Punjab National Bank",
        weight: 1.0,
        phrases: &[
            "punjab national bank",
            "pnb.co.in",
            "pnbindia.in",
            "pnb bank",
            "pnb bank limited",
        ],
    },
    PhraseEntry {
        bank: "Bank of Baroda",
        weight: 1.0,
        phrases: &["bank of baroda", "bankofbaroda.in", "bankofbaroda.com"],
    },
    // "bank of india" substring banks MUST come before Bank of India
    PhraseEntry {
        bank: "Central Bank of India",
        weight: 1.0,
        phrases: &[
            "central bank of india",
            "centralbankofindia.co.in",
            "central bank",
        ],
    },
    PhraseEntry {
        bank: "Union Bank of India",
        weight: 1.0,
        phrases: &[
            "union bank of india",
            "unionbankofindia.co.in",
            "union bank",
        ],
    },
    PhraseEntry {
        bank: "Indian Overseas Bank",
        weight: 1.0,
        phrases: &["indian overseas bank", "iob.in", "iob bank"],
    },
    PhraseEntry {
        bank: "Indian Bank",
        weight: 0.9,
        phrases: &["indian bank", "indianbank.in"],
    },
    PhraseEntry {
        bank: "Bank of India",
        weight: 0.9,
        phrases: &["bank of india", "bankofindia.co.in", "bank of india ltd"],
    },
    PhraseEntry {
        bank: "Bank of Maharashtra",
        weight: 1.0,
        phrases: &["bank of maharashtra", "bankofmaharashtra.in"],
    },
    PhraseEntry {
        bank: "Canara Bank",
        weight: 1.0,
        phrases: &["canara bank", "canarabank.in", "canarabank.com"],
    },
    PhraseEntry {
        bank: "IDFC First Bank",
        weight: 1.0,
        phrases: &["idfc first bank", "idfcfirstbank.com", "idfc bank"],
    },
    PhraseEntry {
        bank: "IDBI Bank",
        weight: 1.0,
        phrases: &["idbi bank", "idbi.com", "industrial development bank"],
    },
    PhraseEntry {
        bank: "YES Bank",
        weight: 1.0,
        phrases: &["yes bank", "yes bank limited", "yesbank.in"],
    },
    PhraseEntry {
        bank: "RBL Bank",
        weight: 1.0,
        phrases: &["rbl bank", "rblbank.com", "ratnakar bank"],
    },
    PhraseEntry {
        bank: "Federal Bank",
        weight: 1.0,
        phrases: &["federal bank", "federalbank.co.in", "the federal bank"],
    },
    PhraseEntry {
        bank: "IndusInd Bank",
        weight: 1.0,
        phrases: &[
            "indusind bank",
            "indusind.com",
            "induslnd bank",
            "indus ind bank",
        ],
    },
    PhraseEntry {
        bank: "UCO Bank",
        weight: 1.0,
        phrases: &["uco bank", "ucobank.com", "united commercial bank"],
    },
    PhraseEntry {
        bank: "AU Small Finance Bank",
        weight: 1.0,
        phrases: &["au small finance bank", "aubank.in", "au sfb", "au bank"],
    },
    PhraseEntry {
        bank: "Bandhan Bank",
        weight: 1.0,
        phrases: &["bandhan bank", "bandhanbank.com"],
    },
    PhraseEntry {
        bank: "Punjab and Sind Bank",
        weight: 1.0,
        phrases: &["punjab and sind bank", "psbindia.com"],
    },
    PhraseEntry {
        bank: "South Indian Bank",
        weight: 1.0,
        phrases: &["south indian bank", "sib.co.in", "the south indian bank"],
    },
    PhraseEntry {
        bank: "Karnataka Bank",
        weight: 1.0,
        phrases: &["karnataka bank", "karnatakabank.com"],
    },
    PhraseEntry {
        bank: "City Union Bank",
        weight: 1.0,
        phrases: &["city union bank", "cityunionbank.com"],
    },
    PhraseEntry {
        bank: "Tamilnad Mercantile Bank",
        weight: 1.0,
        phrases: &["tamilnad mercantile", "tmb bank", "tmbank.in"],
    },
    PhraseEntry {
        bank: "Lakshmi Vilas Bank",
        weight: 1.0,
        phrases: &["lakshmi vilas bank", "lvbank.in"],
    },
    PhraseEntry {
        bank: "Nainital Bank",
        weight: 1.0,
        phrases: &["nainital bank", "nainitalbank.co.in"],
    },
    PhraseEntry {
        bank: "J&K Bank",
        weight: 1.0,
        phrases: &[
            "jammu and kashmir bank",
            "j&k bank",
            "jkbank.com",
            "j & k bank",
        ],
    },
    PhraseEntry {
        bank: "DCB Bank",
        weight: 1.0,
        phrases: &["dcb bank", "dcbbank.in"],
    },
    PhraseEntry {
        bank: "Equitas Small Finance Bank",
        weight: 1.0,
        phrases: &["equitas small finance", "equitas bank", "equitasbank.com"],
    },
    PhraseEntry {
        bank: "Ujjivan Small Finance Bank",
        weight: 1.0,
        phrases: &["ujjivan small finance", "ujjivan bank"],
    },
    PhraseEntry {
        bank: "Jana Small Finance Bank",
        weight: 1.0,
        phrases: &["jana small finance", "jana bank", "janabank.in"],
    },
    PhraseEntry {
        bank: "Cosmos Co-operative Bank",
        weight: 1.0,
        phrases: &[
            "cosmos co-operative bank",
            "cosmos co operative bank",
            "cosmos bank",
            "cosmos co-op",
            "cosmosbank.com",
            "cosmos cooperative",
        ],
    },
    PhraseEntry {
        bank: "Saraswat Co-op Bank",
        weight: 1.0,
        phrases: &[
            "saraswat co op",
            "saraswat co-op",
            "saraswat bank",
            "saraswatbank.com",
            "saraswat cooperative",
            "saraswat",
            "scbl",
        ],
    },
    PhraseEntry {
        // Real fixture (2026-08-30, "Mahanager Co-operative bank.pdf"):
        // every page of the actual file is pure transaction-table body —
        // confirmed by rendering all 5 pages to images and reading them
        // directly — with no header, no footer, no letterhead, and no PDF
        // metadata (Title/Author/Subject are all absent; Producer is just
        // "iText", a generic PDF-generation library) carrying the bank's
        // own identity anywhere. The account's own IFSC/branch never
        // appears in extractable text either, so P1/P2/P3/P4/P5 (header-
        // and body-text tiers) all have nothing to find — filename (P6) is
        // the only evidence source this file actually has, which is why
        // this entry exists even though the phrase never appears in the
        // statement body itself.
        //
        // The filename itself carries a common real-world misspelling —
        // "Mahanager" (missing the second "a") instead of "Mahanagar" —
        // so both spellings are listed; the misspelled variant is a
        // legitimate lexical form of this bank's own name (the same kind
        // of tolerance HDFC's "hdfcbk" or ICICI's "icicibank" entries
        // already give their own bank), not a hardcoded one-off.
        //
        // Deliberately does NOT include "ubin" or any fragment of this
        // account's own linked Union Bank account number/IFSC prefix,
        // which repeats constantly through this statement's narration
        // ("SAVINGS 410702010405405 UBIN") as the destination of the
        // customer's own IMPS self-transfers — that is counterparty
        // evidence about a *different* bank, not this statement's own
        // identity, and must never be treated as a match for either bank.
        bank: "Mahanagar Co-operative Bank",
        weight: 1.0,
        phrases: &[
            "mahanagar co-operative bank",
            "mahanagar co operative bank",
            "mahanagar cooperative bank",
            "mahanagar co-op",
            "mahanagar co op",
            "mahanagar bank",
            "gs mahanagar",
            "mahanager co-operative bank",
            "mahanager co operative bank",
            "mahanager cooperative bank",
            "mahanager bank",
        ],
    },
    PhraseEntry {
        bank: "Paytm Payments Bank",
        weight: 1.0,
        phrases: &["paytm payments bank", "paytmbank.com", "paytm bank"],
    },
    PhraseEntry {
        bank: "Airtel Payments Bank",
        weight: 1.0,
        phrases: &["airtel payments bank", "airtelbank.com", "airtel bank"],
    },
    PhraseEntry {
        bank: "Fino Payments Bank",
        weight: 1.0,
        phrases: &["fino payments bank", "finobank.com"],
    },
    PhraseEntry {
        bank: "India Post Payments Bank",
        weight: 1.0,
        phrases: &["india post payments bank", "ippb.gov.in"],
    },
    PhraseEntry {
        bank: "HSBC Bank",
        weight: 1.0,
        phrases: &["hsbc bank", "hsbc india", "hsbc.co.in"],
    },
    PhraseEntry {
        bank: "Standard Chartered Bank",
        weight: 1.0,
        phrases: &["standard chartered", "standardchartered.co.in"],
    },
    PhraseEntry {
        bank: "Citibank",
        weight: 1.0,
        phrases: &["citibank", "citi bank", "citiindia.com"],
    },
];

// ── OCR abbreviation entries ──────────────────────────────────────────────────

struct AbbrevEntry {
    abbrev: &'static str,
    bank: &'static str,
    max_dist: usize,
}

static OCR_ABBREVS: &[AbbrevEntry] = &[
    AbbrevEntry {
        abbrev: "SBI",
        bank: "State Bank of India",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "HDFC",
        bank: "HDFC Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "ICICI",
        bank: "ICICI Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "AXIS",
        bank: "Axis Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "PNB",
        bank: "Punjab National Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "BOB",
        bank: "Bank of Baroda",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "BOI",
        bank: "Bank of India",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "BOM",
        bank: "Bank of Maharashtra",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "IOB",
        bank: "Indian Overseas Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "KOTAK",
        bank: "Kotak Mahindra Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "IDFC",
        bank: "IDFC First Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "IDBI",
        bank: "IDBI Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "UCO",
        bank: "UCO Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "CBI",
        bank: "Central Bank of India",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "RBL",
        bank: "RBL Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "DCB",
        bank: "DCB Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "TMB",
        bank: "Tamilnad Mercantile Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "JKB",
        bank: "J&K Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "SIB",
        bank: "South Indian Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "KBL",
        bank: "Karnataka Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        abbrev: "CUB",
        bank: "City Union Bank",
        max_dist: 1,
    },
    AbbrevEntry {
        // Not exercised by the real fixture this bank was added for (its
        // header text is missing entirely — see the Mahanagar PHRASE_MAP
        // entry's doc comment) — this is for a future scanned/OCR'd
        // Mahanagar statement whose header *does* survive, same as every
        // other short-code entry in this table.
        abbrev: "MCBL",
        bank: "Mahanagar Co-operative Bank",
        max_dist: 1,
    },
];

// ── Domain / structural patterns ──────────────────────────────────────────────

struct StructEntry {
    pattern: &'static str,
    bank: &'static str,
    conf: f64,
}

static STRUCT_PATTERNS: &[StructEntry] = &[
    StructEntry {
        pattern: r"hdfcbank\.com|hdfc\.com",
        bank: "HDFC Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"sbi\.co\.in|onlinesbi\.com",
        bank: "State Bank of India",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"icicibank\.com",
        bank: "ICICI Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"axisbank\.com",
        bank: "Axis Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"kotakbank\.com",
        bank: "Kotak Mahindra Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"pnb\.co\.in|pnbindia\.in",
        bank: "Punjab National Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"bankofbaroda\.in",
        bank: "Bank of Baroda",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"bankofindia\.co\.in",
        bank: "Bank of India",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"canarabank\.in",
        bank: "Canara Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"unionbankofindia\.co\.in",
        bank: "Union Bank of India",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"idfcfirstbank\.com",
        bank: "IDFC First Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"yesbank\.in",
        bank: "YES Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"rblbank\.com",
        bank: "RBL Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"federalbank\.co\.in",
        bank: "Federal Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"indusind\.com",
        bank: "IndusInd Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"aubank\.in",
        bank: "AU Small Finance Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"bandhanbank\.com",
        bank: "Bandhan Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"cosmosbank\.com",
        bank: "Cosmos Co-operative Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"saraswatbank\.com",
        bank: "Saraswat Co-op Bank",
        conf: 0.95,
    },
    StructEntry {
        pattern: r"jkbank\.com",
        bank: "J&K Bank",
        conf: 0.95,
    },
];

// Compiled structural regexes (built once).
static STRUCT_COMPILED: Lazy<Vec<(Regex, &'static str, f64)>> = Lazy::new(|| {
    STRUCT_PATTERNS
        .iter()
        .map(|e| {
            (
                Regex::new(&format!("(?i){}", e.pattern)).expect("bad struct pattern"),
                e.bank,
                e.conf,
            )
        })
        .collect()
});

// IFSC regex: 4 alpha + '0' + 6 alphanumeric.
static IFSC_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b([A-Z]{4})0[A-Z0-9]{6}\b").unwrap());

// Labeled IFSC: "IFSC: HDFC0001234" style.
static IFSC_LABELED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:ifsc|branch\s*ifsc|ifsc\s*code)\s*[:\-]?\s*([A-Z]{4})0[A-Z0-9]{6}").unwrap()
});

// Metadata extraction regexes.
static ACCT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:a(?:ccount|cct|\/c)[\s\.\-]*(?:no|number|num|#)?)[\s:\-]*([0-9]{6,20})")
        .unwrap()
});
static IFSC_LBL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:ifsc|branch\s+ifsc|ifsc\s+code)[\s:\-]*([A-Z]{4}0[A-Z0-9]{6})").unwrap()
});
// Rust regex does not support lookaheads; stop naturally at newlines/commas.
static BRANCH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:branch\s*(?:name|:)|home\s*branch)[\s:\-]+([A-Za-z][^\n\r,]{2,49})")
        .unwrap()
});
static PERIOD_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:(?:statement|from)\s*(?:date|period)?[\s:\-]+|period[\s:\-]+)(\d{1,2}[\/\-\.]\d{1,2}[\/\-\.]\d{2,4})\s*(?:to|[-–])\s*(\d{1,2}[\/\-\.]\d{1,2}[\/\-\.]\d{2,4})").unwrap()
});
static STMT_ID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:statement\s*(?:id|number|ref(?:erence)?)[\s:\-]+)([A-Z0-9\/\-]{6,30})")
        .unwrap()
});

// ── OCR character repair map ──────────────────────────────────────────────────
// Applied only to short all-caps tokens (potential bank codes).

fn repair_abbrev(tok: &str) -> String {
    if tok.len() < 2 || tok.len() > 8 {
        return tok.to_string();
    }
    tok.chars()
        .map(|c| match c {
            'O' | 'o' => '0',
            'l' => '1',
            'S' => '5',
            'Z' => '2',
            'B' => '8',
            'G' => '6',
            'I' => '1',
            other => other,
        })
        .collect()
}

// ── Levenshtein distance ──────────────────────────────────────────────────────

pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for (i, row) in dp.iter_mut().enumerate().take(m + 1) {
        row[0] = i;
    }
    for (j, cell) in dp[0].iter_mut().enumerate().take(n + 1) {
        *cell = j;
    }
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1]
            } else {
                1 + dp[i - 1][j].min(dp[i][j - 1]).min(dp[i - 1][j - 1])
            };
        }
    }
    dp[m][n]
}

// ── Normalize text for comparison ─────────────────────────────────────────────
// Lower-case, remove special chars except a-z 0-9 space dot @, collapse whitespace.

pub fn norm(s: &str) -> String {
    let s = s.to_lowercase();
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '@' {
                c
            } else {
                ' '
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── Extract IFSC prefix codes from text ──────────────────────────────────────

pub fn extract_ifscs(text: &str) -> Vec<String> {
    IFSC_RE
        .captures_iter(text)
        .map(|cap| cap[1].to_uppercase())
        .collect()
}

// ── Internal detection result ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DetectHit {
    bank: &'static str,
    confidence: f64,
    method: &'static str,
    ifsc: Option<String>,
}

// `norm()` maps every separator (spaces, slashes, dashes, ...) to a plain
// space and leaves alphanumerics untouched, so a phrase can end up sitting
// in the middle of a longer glued alphanumeric run that was never meant to
// be one word at all — e.g. a "BULK POSTING-ACHCr...HDFCBANKLTD-" narration
// (a *counterparty* literally named "HDFC BANK LTD" in an ACH credit,
// nothing to do with whose statement this is) collapses to
// "...hdfcbankltd...", and a plain substring search happily reports a
// "hdfcbank" phrase match inside it. Requiring real word boundaries on both
// sides — the same discipline `IFSC_RE` already applies with `\b` — rejects
// that false positive while still matching every legitimate case (a phrase
// surrounded by spaces, or by the `.`/`@` characters `norm()` deliberately
// preserves for domain-style phrases like "hdfcbank.com").
fn find_word_bounded(text: &str, phrase: &str) -> Option<usize> {
    let is_word = |c: char| c.is_ascii_alphanumeric();
    let mut start = 0;
    while let Some(rel) = text[start..].find(phrase) {
        let pos = start + rel;
        let before_ok = text[..pos].chars().next_back().is_none_or(|c| !is_word(c));
        let after_ok = text[pos + phrase.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_word(c));
        if before_ok && after_ok {
            return Some(pos);
        }
        start = pos + 1;
        if start >= text.len() {
            break;
        }
    }
    None
}

// A UPI/NEFT/IMPS/RTGS/ECS/NACH/ACH/POS transaction reference — the
// slash-or-colon-delimited blob every Indian bank statement narration packs
// a counterparty's own bank code into ("UPIAB/410969711856/CR/MRRAJES/
// SCBL/9773690640-2@y", "IMPSAR/509113411756/RashiDubey/916010011970001",
// "POS:Bundltechnolog/vBangalore/409601718694"). Matching the reference's
// *shape* rather than relying on the separately-parsed `Transaction::
// narration` field sidesteps a real mismatch: that field has already had
// its "/DR/"/"/CR/" marker stripped by the time bank detection runs (e.g.
// raw "UPIAB/410969711856/CR/MRRAJES/SCBL/..." parses to narration
// "UPIAB/410969711856//MRRAJES/SCBL/..."), so a naive substring-removal
// using the parsed narration would silently fail to match the raw text at
// all and leave the false positive in place.
static TXN_REF_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:UPI|NEFT|IMPS|RTGS|ECS|NACH|ACH|POS)[A-Z]{0,3}[:\-]?/?[A-Za-z0-9@/.\-]{6,100}",
    )
    .unwrap()
});

// Blanks out every transaction-reference-shaped span in `text` before it's
// handed to a whole-document phrase scan (P5) — see that call site's doc
// comment for why. A genuine header/branding phrase is never itself
// preceded by one of these payment-rail prefixes, so this can only ever
// remove narration content, never a real match.
fn strip_transaction_references(text: &str) -> String {
    TXN_REF_RE.replace_all(text, " ").into_owned()
}

fn detect_by_phrase(norm_text: &str) -> Option<DetectHit> {
    let mut best_bank = None;
    let mut best_conf = 0.0f64;
    let mut best_pos = usize::MAX;

    for entry in PHRASE_MAP {
        for phrase in entry.phrases {
            if let Some(pos) = find_word_bounded(norm_text, phrase) {
                if pos < best_pos {
                    best_pos = pos;
                    best_conf = (0.95 * entry.weight).min(0.95);
                    best_bank = Some(entry.bank);
                }
            }
        }
    }

    best_bank.map(|bank| DetectHit {
        bank,
        confidence: best_conf,
        method: "phrase",
        ifsc: None,
    })
}

fn detect_by_ifsc(text: &str) -> Option<DetectHit> {
    // Labeled IFSC first (highest confidence).
    if let Some(cap) = IFSC_LABELED_RE.captures(text) {
        let code = cap[1].to_uppercase();
        if let Some(&bank) = IFSC_MAP.get(code.as_str()) {
            let full_ifsc = IFSC_RE.find(text).map(|m| m.as_str().to_uppercase());
            return Some(DetectHit {
                bank,
                confidence: 0.98,
                method: "ifsc_labeled",
                ifsc: full_ifsc,
            });
        }
    }
    // Any IFSC in text — lower confidence.
    for code in extract_ifscs(text) {
        if let Some(&bank) = IFSC_MAP.get(code.as_str()) {
            return Some(DetectHit {
                bank,
                confidence: 0.80,
                method: "ifsc_any",
                ifsc: None,
            });
        }
    }
    None
}

fn detect_by_fuzzy(text: &str) -> Option<DetectHit> {
    let upper = text.to_uppercase();
    let words: Vec<&str> = upper
        .split(|c: char| " /\\-|,._@".contains(c))
        .filter(|w| w.len() >= 3 && w.len() <= 8)
        .collect();

    for raw in &words {
        // Raw token first.
        for entry in OCR_ABBREVS {
            let dist = levenshtein(raw, entry.abbrev);
            if dist <= entry.max_dist {
                let conf = if *raw == entry.abbrev { 0.85 } else { 0.72 };
                let method = if *raw == entry.abbrev {
                    "abbrev_exact"
                } else {
                    "abbrev_fuzzy"
                };
                return Some(DetectHit {
                    bank: entry.bank,
                    confidence: conf,
                    method,
                    ifsc: None,
                });
            }
        }
        // OCR-repaired token.
        let repaired = repair_abbrev(raw);
        if repaired != *raw {
            for entry in OCR_ABBREVS {
                if levenshtein(&repaired, entry.abbrev) <= entry.max_dist {
                    return Some(DetectHit {
                        bank: entry.bank,
                        confidence: 0.72,
                        method: "abbrev_fuzzy_repaired",
                        ifsc: None,
                    });
                }
            }
        }
    }
    None
}

fn detect_by_filename(filename: &str) -> Option<DetectHit> {
    let normed = norm(filename);
    let hit = detect_by_phrase(&normed).or_else(|| detect_by_fuzzy(filename))?;
    Some(DetectHit {
        bank: hit.bank,
        confidence: hit.confidence.min(0.65),
        method: "filename",
        ifsc: None,
    })
}

fn detect_by_narrations(narrations: &[&str]) -> Option<DetectHit> {
    if narrations.is_empty() {
        return None;
    }
    let sample = narrations
        .iter()
        .take(50)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let codes = extract_ifscs(&sample);
    if codes.is_empty() {
        return None;
    }

    let mut freq: HashMap<String, usize> = HashMap::new();
    for c in &codes {
        *freq.entry(c.clone()).or_insert(0) += 1;
    }

    let top = freq.iter().max_by_key(|(_, &v)| v)?;
    let (code, &count) = top;
    if count < 2 {
        return None;
    }
    let bank = *IFSC_MAP.get(code.as_str())?;
    Some(DetectHit {
        bank,
        confidence: 0.55,
        method: "narration_ifsc",
        ifsc: None,
    })
}

fn detect_by_structure(text: &str) -> Option<DetectHit> {
    for (re, bank, conf) in STRUCT_COMPILED.iter() {
        if re.is_match(text) {
            return Some(DetectHit {
                bank,
                confidence: *conf,
                method: "domain",
                ifsc: None,
            });
        }
    }
    None
}

// ── Metadata extraction ───────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct StatementMeta {
    pub account_no: String,
    pub ifsc: String,
    pub branch: String,
    pub statement_period: String,
    pub statement_id: String,
}

fn extract_meta(text: &str) -> StatementMeta {
    let mut meta = StatementMeta::default();

    if let Some(cap) = ACCT_RE.captures(text) {
        let raw = cap[1].trim().to_string();
        meta.account_no = if raw.len() > 8 {
            format!(
                "{}{}{}",
                &raw[..4],
                "X".repeat(raw.len() - 8),
                &raw[raw.len() - 4..]
            )
        } else {
            raw
        };
    }

    if let Some(cap) = IFSC_LBL_RE.captures(text) {
        meta.ifsc = cap[1].trim().to_string();
    }

    if let Some(cap) = BRANCH_RE.captures(text) {
        meta.branch = cap[1].split_whitespace().collect::<Vec<_>>().join(" ");
    }

    if let Some(cap) = PERIOD_RE.captures(text) {
        meta.statement_period = format!("{} to {}", &cap[1], &cap[2]);
    }

    if let Some(cap) = STMT_ID_RE.captures(text) {
        meta.statement_id = cap[1].trim().to_string();
    }

    meta
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Result of bank detection.
#[derive(Debug, Clone, Default)]
pub struct BankDetectionResult {
    pub bank_name: String,
    pub confidence: f64,
    pub method: String,
    pub account_no: String,
    pub ifsc: String,
    pub branch: String,
    pub statement_period: String,
    pub statement_id: String,
    /// true when confidence < 0.6 and a bank was found.
    pub needs_review: bool,
    pub source_file: String,
}

/// Options for `detect()`.
#[derive(Default)]
pub struct DetectOptions<'a> {
    /// Full raw text of the statement (headers + body).
    pub text: &'a str,
    /// Text from rows above the column-header row (subset of `text`).
    pub header_text: &'a str,
    /// Original file name.
    pub filename: &'a str,
    /// Narration strings (for IFSC frequency scan).
    pub narrations: &'a [&'a str],
}

/// Detect bank name from any combination of text sources.
/// Mirrors `BankDetectionEngine.detect()` exactly (same priority order).
pub fn detect(opts: DetectOptions<'_>) -> BankDetectionResult {
    let DetectOptions {
        text,
        header_text,
        filename,
        narrations,
    } = opts;
    let mut result: Option<DetectHit> = None;

    macro_rules! update {
        ($hit:expr) => {{
            if let Some(h) = $hit {
                if result
                    .as_ref()
                    .map_or(true, |r| h.confidence > r.confidence)
                {
                    result = Some(h);
                }
            }
        }};
    }

    // P1: labeled IFSC in header text (0.98)
    if !header_text.is_empty() {
        update!(detect_by_ifsc(header_text));
    }

    // P2: domain / email pattern in full text (0.95)
    if result.as_ref().is_none_or(|r| r.confidence < 0.95) {
        update!(detect_by_structure(text));
    }

    // P3: phrase match in header text (0.95)
    if result.as_ref().is_none_or(|r| r.confidence < 0.95) {
        update!(detect_by_phrase(&norm(header_text)));
    }

    // P4: fuzzy abbreviation in header text (0.72–0.85)
    if result.as_ref().is_none_or(|r| r.confidence < 0.75) {
        update!(detect_by_fuzzy(header_text));
    }

    // P5: phrase match in full text, confidence capped at 0.80 — scanned
    // with every known transaction narration stripped out first (see
    // `strip_narrations`'s doc comment). A counterparty's bank code sitting
    // inside a UPI/NEFT/IMPS reference ("UPIAR/.../DR/NAME/SCBL/handle") is
    // otherwise indistinguishable, to a plain phrase scan, from the
    // statement's own real letterhead — and at this tier's 0.80 cap it
    // would outrank filename detection (P6, capped 0.65) and even the
    // narration-aware IFSC-frequency tier built for exactly this evidence
    // class (P7, capped 0.55). Narration-derived evidence must never be
    // stronger than either of those, per this tier's whole reason for
    // existing below P3/P4: real header/branding text — genuinely
    // independent of any single transaction — still matches here (it isn't
    // narration content, so stripping narrations leaves it untouched).
    if result.as_ref().is_none_or(|r| r.confidence < 0.82) {
        let sans_refs = strip_transaction_references(text);
        if let Some(h) = detect_by_phrase(&norm(&sans_refs)) {
            let adj = h.confidence.min(0.80);
            if result.as_ref().is_none_or(|r| adj > r.confidence) {
                result = Some(DetectHit {
                    confidence: adj,
                    method: "phrase_full",
                    ..h
                });
            }
        }
    }

    // P6: filename (capped at 0.65)
    if result.as_ref().is_none_or(|r| r.confidence < 0.70) {
        update!(detect_by_filename(filename));
    }

    // P7: narration IFSC frequency (0.55)
    if result.as_ref().is_none_or(|r| r.confidence < 0.60) {
        update!(detect_by_narrations(narrations));
    }

    let meta = extract_meta(if !text.is_empty() { text } else { header_text });

    // Final override: labeled IFSC in header at 0.98 beats anything lower.
    if !header_text.is_empty() {
        if let Some(h) = detect_by_ifsc(header_text) {
            if h.confidence >= 0.98 && result.as_ref().is_none_or(|r| h.confidence > r.confidence) {
                result = Some(h);
            }
        }
    }

    let bank_name = result.as_ref().map_or("", |r| r.bank).to_string();
    let confidence = result.as_ref().map_or(0.0, |r| r.confidence);
    let method = result.as_ref().map_or("none", |r| r.method).to_string();
    let ifsc_val = result
        .as_ref()
        .and_then(|r| r.ifsc.clone())
        .unwrap_or_else(|| meta.ifsc.clone());

    BankDetectionResult {
        needs_review: confidence < 0.6 && !bank_name.is_empty(),
        bank_name,
        confidence,
        method,
        account_no: meta.account_no,
        ifsc: ifsc_val,
        branch: meta.branch,
        statement_period: meta.statement_period,
        statement_id: meta.statement_id,
        source_file: filename.to_string(),
    }
}

/// Lightweight convenience — check just a text string (no metadata).
pub fn match_bank(text: &str) -> String {
    let normed = norm(text);
    detect_by_phrase(&normed)
        .or_else(|| detect_by_ifsc(text))
        .or_else(|| detect_by_fuzzy(text))
        .map(|h| h.bank.to_string())
        .unwrap_or_default()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── levenshtein ───────────────────────────────────────────────────────────

    #[test]
    fn lev_identical() {
        assert_eq!(levenshtein("SBI", "SBI"), 0);
    }

    #[test]
    fn lev_one_insert() {
        assert_eq!(levenshtein("SBI", "SBIS"), 1);
    }

    #[test]
    fn lev_one_delete() {
        assert_eq!(levenshtein("SBIX", "SBI"), 1);
    }

    #[test]
    fn lev_one_replace() {
        assert_eq!(levenshtein("SBL", "SBI"), 1);
    }

    #[test]
    fn lev_empty_a() {
        assert_eq!(levenshtein("", "ABC"), 3);
    }

    #[test]
    fn lev_empty_b() {
        assert_eq!(levenshtein("ABC", ""), 3);
    }

    // ── norm ──────────────────────────────────────────────────────────────────

    #[test]
    fn norm_lowercases_and_strips() {
        assert_eq!(norm("HDFC Bank Ltd!"), "hdfc bank ltd");
    }

    #[test]
    fn norm_collapses_whitespace() {
        assert_eq!(norm("  State   Bank  "), "state bank");
    }

    // ── extract_ifscs ─────────────────────────────────────────────────────────

    #[test]
    fn extracts_ifsc_code() {
        let codes = extract_ifscs("Branch IFSC: HDFC0001234");
        assert!(codes.contains(&"HDFC".to_string()));
    }

    #[test]
    fn extracts_multiple_ifscs() {
        let codes = extract_ifscs("SBIN0001234 and ICIC0005678");
        assert!(codes.contains(&"SBIN".to_string()));
        assert!(codes.contains(&"ICIC".to_string()));
    }

    // ── detect_by_ifsc ───────────────────────────────────────────────────────

    #[test]
    fn labeled_ifsc_hdfc_conf_098() {
        let hit = detect_by_ifsc("IFSC Code: HDFC0001234").unwrap();
        assert_eq!(hit.bank, "HDFC Bank");
        assert!((hit.confidence - 0.98).abs() < 0.001);
        assert_eq!(hit.method, "ifsc_labeled");
    }

    #[test]
    fn any_ifsc_sbi_conf_080() {
        let hit = detect_by_ifsc("Transfer to SBIN0001234").unwrap();
        assert_eq!(hit.bank, "State Bank of India");
        assert!((hit.confidence - 0.80).abs() < 0.001);
        assert_eq!(hit.method, "ifsc_any");
    }

    #[test]
    fn unknown_ifsc_returns_none() {
        assert!(detect_by_ifsc("XXXX0001234").is_none());
    }

    // ── detect_by_phrase ────────────────────────────────────────────────────

    #[test]
    fn phrase_hdfc_header() {
        let hit = detect_by_phrase("hdfc bank statement jan 2024").unwrap();
        assert_eq!(hit.bank, "HDFC Bank");
        assert!(hit.confidence >= 0.90);
    }

    #[test]
    fn phrase_sbi_wins_over_hdfc_in_narration() {
        // "state bank of india" appears first → SBI wins
        let text = "state bank of india account hdfc bank neft payment";
        let hit = detect_by_phrase(&norm(text)).unwrap();
        assert_eq!(hit.bank, "State Bank of India");
    }

    #[test]
    fn phrase_cosmos_cooperative() {
        let hit = detect_by_phrase("cosmos co-operative bank").unwrap();
        assert_eq!(hit.bank, "Cosmos Co-operative Bank");
    }

    #[test]
    fn phrase_kotak_short() {
        let hit = detect_by_phrase("kotak").unwrap();
        assert_eq!(hit.bank, "Kotak Mahindra Bank");
    }

    // ── detect_by_fuzzy ──────────────────────────────────────────────────────

    #[test]
    fn fuzzy_exact_sbi_conf_085() {
        let hit = detect_by_fuzzy("SBI BRANCH MUMBAI").unwrap();
        assert_eq!(hit.bank, "State Bank of India");
        assert!((hit.confidence - 0.85).abs() < 0.001);
        assert_eq!(hit.method, "abbrev_exact");
    }

    #[test]
    fn fuzzy_sbl_matches_sbi() {
        // Levenshtein("SBL","SBI") = 1 ≤ max_dist 1
        let hit = detect_by_fuzzy("SBL BANK").unwrap();
        assert_eq!(hit.bank, "State Bank of India");
        assert!((hit.confidence - 0.72).abs() < 0.001);
    }

    #[test]
    fn fuzzy_kotak_exact() {
        let hit = detect_by_fuzzy("KOTAK ACCOUNT").unwrap();
        assert_eq!(hit.bank, "Kotak Mahindra Bank");
    }

    // ── detect_by_filename ───────────────────────────────────────────────────

    #[test]
    fn filename_hdfc_statement() {
        let hit = detect_by_filename("HDFC_Bank_Statement_Jan2024.xlsx").unwrap();
        assert_eq!(hit.bank, "HDFC Bank");
        assert!(hit.confidence <= 0.65);
        assert_eq!(hit.method, "filename");
    }

    // ── detect_by_narrations ────────────────────────────────────────────────

    #[test]
    fn narrations_need_two_occurrences() {
        // Only 1 SBIN IFSC → rejected
        let narrs = vec!["NEFT SBIN0001234 payment"];
        assert!(detect_by_narrations(&narrs).is_none());
    }

    #[test]
    fn narrations_two_ifscs_detected() {
        let narrs = vec![
            "NEFT from SBIN0001234 Rajesh",
            "NEFT from SBIN0001234 Priya",
        ];
        let hit = detect_by_narrations(&narrs).unwrap();
        assert_eq!(hit.bank, "State Bank of India");
        assert!((hit.confidence - 0.55).abs() < 0.001);
    }

    // ── detect_by_structure ─────────────────────────────────────────────────

    #[test]
    fn domain_hdfcbank_detected() {
        let hit = detect_by_structure("Please visit hdfcbank.com for details").unwrap();
        assert_eq!(hit.bank, "HDFC Bank");
        assert!((hit.confidence - 0.95).abs() < 0.001);
        assert_eq!(hit.method, "domain");
    }

    #[test]
    fn domain_cosmos_detected() {
        let hit = detect_by_structure("cosmosbank.com customer care").unwrap();
        assert_eq!(hit.bank, "Cosmos Co-operative Bank");
    }

    // ── extract_meta ─────────────────────────────────────────────────────────

    #[test]
    fn meta_extracts_account_no() {
        let meta = extract_meta("Account No: 50100123456789");
        assert!(
            meta.account_no.contains("X"),
            "account should be masked: {}",
            meta.account_no
        );
        assert!(meta.account_no.ends_with("6789"));
    }

    #[test]
    fn meta_extracts_ifsc() {
        let meta = extract_meta("IFSC Code: HDFC0001234");
        assert_eq!(meta.ifsc, "HDFC0001234");
    }

    #[test]
    fn meta_extracts_period() {
        let meta = extract_meta("Statement Period: 01/01/2024 to 31/01/2024");
        assert_eq!(meta.statement_period, "01/01/2024 to 31/01/2024");
    }

    // ── detect() (full pipeline) ─────────────────────────────────────────────

    #[test]
    fn detect_hdfc_full_header() {
        let result = detect(DetectOptions {
            header_text: "HDFC Bank Ltd\nIFSC Code: HDFC0000060\nAccount No: 50100123456789",
            text: "HDFC Bank Ltd\nIFSC Code: HDFC0000060\nAccount No: 50100123456789",
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "HDFC Bank");
        assert!(result.confidence >= 0.95);
        assert!(!result.needs_review);
    }

    #[test]
    fn detect_sbi_labeled_ifsc_wins() {
        // Labeled IFSC = 0.98, beats any phrase match
        let result = detect(DetectOptions {
            header_text: "IFSC Code: SBIN0001234\nState Bank of India",
            text: "IFSC Code: SBIN0001234\nState Bank of India",
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "State Bank of India");
        assert!((result.confidence - 0.98).abs() < 0.001);
        assert_eq!(result.method, "ifsc_labeled");
    }

    #[test]
    fn detect_narration_hdfc_does_not_steal_sbi() {
        // SBI in header, HDFC in narrations → SBI wins (P3 > P7)
        let result = detect(DetectOptions {
            header_text: "State Bank of India\nAccount",
            text: "State Bank of India\nNEFT FROM HDFC BANK CUSTOMER",
            narrations: &["NEFT FROM HDFC BANK CUSTOMER"],
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "State Bank of India");
    }

    #[test]
    fn detect_counterparty_named_hdfc_bank_ltd_does_not_steal_sbi() {
        // Real bug (2026-08-29): an SBI statement with no textual header at
        // all (confirmed against the actual fixture — zero "State Bank"/
        // "IFSC" occurrences anywhere) has a "BULK POSTING-ACHCr" narration
        // whose counterparty is literally a company named "HDFC BANK LTD"
        // — nothing to do with whose statement this is. `norm()` glues the
        // whole narration into one run with no internal separator
        // ("...hdfcbankltd..."), and a plain substring search used to
        // report a "hdfcbank" phrase match inside it (P5, capped 0.80) —
        // high enough to suppress filename-based detection (P6, needs
        // confidence < 0.70 to even attempt), which would otherwise have
        // correctly recognized "SBI" from the filename via fuzzy
        // abbreviation matching. Word-boundary-checked phrase matching
        // rejects the false positive so filename detection gets its turn.
        let result = detect(DetectOptions {
            text: "BULK POSTING-ACHCrHDFC00161000007598HDFCBANKLTD-488.00",
            filename: "SBI.pdf",
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "State Bank of India");
    }

    #[test]
    fn detect_union_bank_not_saraswat_via_narration_counterparty_code() {
        // Real bug (2026-08-30): a Union Bank of India statement with no
        // textual header at all (its own true first page is missing from
        // the fixture, same class of gap as the SBI case above) has an
        // ordinary UPI narration whose *counterparty's* bank is Saraswat
        // Co-op Bank — "UPIAB/410969711856/CR/MRRAJES/SCBL/9773690640-2@y",
        // nothing to do with whose statement this is. Unlike the SBI case,
        // "SCBL" already sits inside clean `/.../` delimiters — word-
        // boundary checking alone does not reject it — so P5 (phrase
        // anywhere in the full document text) reported Saraswat Co-op Bank
        // at 0.80 confidence, high enough to suppress the correct filename-
        // based "Union Bank of India" detection (P6, needs confidence
        // < 0.70 to even attempt). Stripping every UPI/NEFT/IMPS/RTGS/ECS/
        // NACH/ACH/POS transaction-reference-shaped span out of the text
        // before P5 scans it removes this narration entirely, so filename
        // detection gets its turn.
        let result = detect(DetectOptions {
            text: "UPIAB/410969711856/CR/MRRAJES/SCBL/9773690640-2@y",
            filename: "Union Bank.pdf",
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "Union Bank of India");
    }

    #[test]
    fn detect_saraswat_bank_by_phrase_still_works() {
        // Regression guard for the fix above: a *real* Saraswat Co-op Bank
        // statement's own header phrase must still win — stripping
        // transaction-reference-shaped spans out of the text must never
        // remove genuine header/branding prose, which is never itself
        // preceded by a payment-rail prefix.
        let result = detect(DetectOptions {
            header_text: "Saraswat Co-operative Bank Statement",
            text: "Saraswat Co-operative Bank Statement",
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "Saraswat Co-op Bank");
    }

    #[test]
    fn detect_mahanagar_co_operative_bank_via_filename_despite_common_typo() {
        // Real fixture (2026-08-30, "Mahanager Co-operative bank.pdf"):
        // every one of its 5 pages is pure transaction-table body (verified
        // by rendering each page to an image) — no header, no footer, no
        // PDF metadata carrying the bank's own identity anywhere, and no
        // IFSC/branch for *this* account ever appears in extractable text.
        // Filename is the only evidence source this file has at all, and
        // the filename itself carries a common real-world misspelling
        // ("Mahanager", not "Mahanagar") — this locks in that the filename
        // tier (P6) still resolves it correctly despite that typo.
        let result = detect(DetectOptions {
            text: "",
            filename: "Mahanager Co-operative bank.pdf",
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "Mahanagar Co-operative Bank");
    }

    #[test]
    fn detect_mahanagar_not_union_bank_via_self_transfer_narration_counterparty_code() {
        // Same class of trap as `detect_union_bank_not_saraswat_via_
        // narration_counterparty_code` above, found in the same real
        // Mahanagar fixture: this account makes frequent IMPS transfers to
        // the customer's *own* linked Union Bank of India savings account,
        // so "SAVINGS 410702010405405 UBIN" (Union Bank's own IFSC prefix)
        // repeats throughout the narration body — evidence about a
        // *different* bank entirely, not this statement's own identity.
        // Confirms the fix doesn't accidentally start matching "Mahanagar"
        // off narration content either — filename remains the only signal
        // that wins here, and the counterparty reference must not cause a
        // false "Union Bank of India" detection.
        let result = detect(DetectOptions {
            text: "IMPS P2A 427402304589 SAVINGS 410702010405405 UBIN Rs 100000.00",
            filename: "Mahanager Co-operative bank.pdf",
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "Mahanagar Co-operative Bank");
    }

    #[test]
    fn detect_cosmos_bank_by_phrase() {
        let result = detect(DetectOptions {
            header_text: "Cosmos Co-operative Bank",
            text: "Cosmos Co-operative Bank Statement",
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "Cosmos Co-operative Bank");
    }

    #[test]
    fn detect_no_bank_returns_empty() {
        let result = detect(DetectOptions {
            text: "random text without any bank name",
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "");
        assert_eq!(result.confidence, 0.0);
        assert_eq!(result.method, "none");
        assert!(!result.needs_review);
    }

    #[test]
    fn detect_low_confidence_sets_needs_review() {
        // Narration IFSC scan → conf=0.55 < 0.6 → needs_review=true.
        // (Filename-only returns 0.65 which is ≥ 0.6, so does NOT trigger needs_review per JS.)
        let narrs = vec![
            "NEFT from SBIN0001234 Rajesh",
            "NEFT from SBIN0001234 Priya",
        ];
        let result = detect(DetectOptions {
            narrations: &narrs,
            ..DetectOptions::default()
        });
        assert_eq!(result.bank_name, "State Bank of India");
        assert!(
            result.needs_review,
            "conf={} should trigger needs_review",
            result.confidence
        );
    }

    #[test]
    fn match_bank_returns_name() {
        assert_eq!(match_bank("HDFC Bank Statement"), "HDFC Bank");
    }

    #[test]
    fn match_bank_unknown_returns_empty() {
        assert_eq!(match_bank("random unrelated text"), "");
    }
}
