# Third-party data in the secrets test set

`test.tsv` in this directory is drawn from five third-party sources, all synthetic: none of them holds
real credentials or real people's data by its authors' account. This file gives the attribution each
licence requires and says what was taken and how it was changed. The licence texts are in
[`licenses/`](licenses/). The rest of this directory (`train.tsv`, `dev.tsv`, `labels.md`) is wisp's own
work under the repository's licence and takes nothing from these sources.

## What was done to every line

- **Taken as lines.** Each line of `test.tsv` is one line, or one sentence, of a source record. Multi-line
  records (files, documents, JSON objects) were cut down to the one line that carries the value; a few
  lines lost a leading marker such as `TP:`, `POSITIVE:`, or `detected:`, or a trailing comment, and
  terminal colour codes and tab characters were removed.
- **Relabelled.** Every line was labelled `secret`, `personal`, or `none` by the definitions and rules in
  [`labels.md`](labels.md). No source's own flag was copied: a scanner fixture records whether a pattern
  should match, not what the text is, so gitleaks' "false positives" and CredSweeper's negatives were
  judged afresh (some are secrets), and PII spans were read against wisp's rules (a server's IP, a
  company name, or a date that is not a birth date is `none`).
- **Marked for scanners.** A zero-width space, U+200B, was inserted after the fourth character of each
  value shaped like a credential (keys, tokens, passwords, JWT segments, private-key lines, and
  placeholders that secret scanners match), and inside private-key headers, so that GitHub push
  protection and secret scanners do not take the file for a leak. Loaders strip U+200B before use.
- **Email domains.** Personal email addresses on real mail domains (gmail.com, ymail.com, icloud.com,
  hotmail.com, and similar) were moved to example.com, example.net, or example.org, keeping the local
  part.
- **Values that could be real, neutralised.** The sources' values are synthetic but not always from
  reserved ranges, so some could match a real person. Such values were moved to the conventions of
  `labels.md` rule 1:
  - Social Security numbers were moved to the unissued 9xx area.
  - US phone numbers were moved to the fictional 555-01xx block.
  - Card numbers were replaced by obvious sequences of the same brand prefix and length, still
    Luhn-valid.
  - IBAN check digits were set to 00, which no valid IBAN has.
  - Routable IP addresses were moved to the documentation ranges.
- **Removed.** The source's own verdicts left in a line ("POSITIVE", "# FP", "should be not found"), a
  token that gitleaks took from a public tutorial, and any value drawn from FakeNameGenerator records
  (Privy's state, city, ZIP, and card expiry fields) were removed. The review is
  `../reviews/secrets-test.md`.
- **Selected and deduplicated.** Only a small sample of each source was read, lines that were duplicates
  or near duplicates of each other or of `train.tsv` and `dev.tsv` were dropped, and no provider's token
  format appears more than about three times.

## Sources

### Samsung CredSweeper, `tests/samples`

- URL: <https://github.com/Samsung/CredSweeper/tree/main/tests/samples>, commit
  `f21ab2f2553eea288a72273b9658cd297ab1d11f` (2026-09-24).
- Licence: MIT. Copyright (c) 2021 SAMSUNG. Text: [`licenses/CredSweeper-MIT.txt`](licenses/CredSweeper-MIT.txt).
- Taken: lines of the text sample files (credentials in config, code, shell, SQL, documents, and mail;
  their negative counterparts), about 200 lines of `test.tsv`, including 40 in the hard-negative slice.
- Changed: as above; the samples' positive and negative file names were not used as labels.

### gitleaks rule fixtures, `cmd/generate/config/rules/*.go`

- URL: <https://github.com/gitleaks/gitleaks/tree/master/cmd/generate/config/rules>, commit
  `b58d3f102cf3a2c84cb7f923d05c25c9b1aed84b` (2026-07-22).
- Licence: MIT. Copyright (c) 2019 Zachary Rice. Text: [`licenses/gitleaks-MIT.txt`](licenses/gitleaks-MIT.txt).
- Taken: string literals of the rules' `tps` and `fps` arrays, and two lines formed by the fixtures'
  own `GenerateSampleSecrets` templates from literal sample values in `generic.go`; about 130 lines,
  80 of them in the hard-negative slice. Literals that could be real, published keys (the `gcp.go`
  allowlisted keys, tokens lifted from public repositories) were left out.
- Changed: as above; `tps` and `fps` were relabelled, not copied.

### NVIDIA Nemotron-PII

- URL: <https://huggingface.co/datasets/nvidia/Nemotron-PII>, revision
  `b70ffaf5ff39e079776134c5bf4381f00a9fd1ed`; test split, rows 0 to 199, read through the Hugging Face
  dataset viewer API.
- Licence: Creative Commons Attribution 4.0 International (CC BY 4.0),
  <https://creativecommons.org/licenses/by/4.0/legalcode>. Text: [`licenses/CC-BY-4.0.txt`](licenses/CC-BY-4.0.txt).
- Attribution: "Nemotron-PII: Synthesized Data for Privacy-Preserving AI" by Amy Steier, Andre Manoel,
  Alexa Haushalter, and Maarten Van Segbroeck, NVIDIA, 2025, licensed under CC BY 4.0.
- Taken: 83 lines or sentences of the synthetic documents.
- Changed: this is an adapted subset. Documents were split into lines, lines were selected and
  relabelled, email domains were moved to example domains, and U+200B was inserted into credential-shaped
  values, all as described above. No endorsement by NVIDIA or the authors is implied. The adapted lines
  remain under CC BY 4.0.

### Gretel `gretel-pii-masking-en-v1`

- URL: <https://huggingface.co/datasets/gretelai/gretel-pii-masking-en-v1>, revision
  `e06eb1499ca8d54470f085021cd8e54f9efac7fd`; test split, rows 0 to 199.
- Licence: Apache License 2.0. Gretel AI, "GLiNER Models for PII Detection through Fine-Tuning on
  Gretel-Generated Synthetic Documents", 2024. Text: [`licenses/Apache-2.0.txt`](licenses/Apache-2.0.txt).
  The dataset ships no NOTICE file.
- Taken: 132 lines or sentences of the synthetic documents.
- Changed: split into lines, selected, relabelled, and marked with U+200B as described above.

### Gretel `synthetic_pii_finance_multilingual`

- URL: <https://huggingface.co/datasets/gretelai/synthetic_pii_finance_multilingual>, revision
  `7b844d16738527a04264f50214cb426a4cea0897`; test split, the English rows among rows 0 to 199.
- Licence: Apache License 2.0. Alex Watson, Yev Meyer, Maarten Van Segbroeck, Matthew Grossman, Sami
  Torbey, Piotr Mlocek, and Johnny Greco, Gretel, 2024. Text: [`licenses/Apache-2.0.txt`](licenses/Apache-2.0.txt).
  The dataset ships no NOTICE file.
- Taken: 76 lines of the synthetic financial documents (forms, EDI, SWIFT and MT940 messages, XBRL,
  support logs).
- Changed: split into lines, selected, relabelled, email domains moved to example domains, and marked with
  U+200B as described above.

### Privy (`beki/privy`)

- URL: <https://huggingface.co/datasets/beki/privy>, revision `dc137a6a976f6b5bb8768e9bb51ec58df930ccd1`;
  the first records of `test-small.json` inside `privy-dataset.zip`, read by HTTP range request without
  downloading the archive. Generator: <https://github.com/pixie-io/pixie/tree/main/src/datagen/pii/privy>.
- Licence: MIT, as the dataset card declares; author Benjamin Kilimnik (2022). Text:
  [`licenses/Privy-MIT.txt`](licenses/Privy-MIT.txt).
- Taken: 28 protocol-trace lines (JSON, SQL, HTML, XML). Privy fills names, emails, addresses, phone
  numbers, national ids, and passwords from presidio-research's FakeNameGenerator.com records, which are
  under CC BY-SA 3.0, so no line carrying those fields was used: only lines whose values come from Faker
  or Privy's own providers (IBAN, BBAN, bank account and routing numbers, driver's licence, passport,
  ITIN, IMEI) and lines with no personal data.
- Changed: selected and relabelled as described above; the records' span annotations were not used as
  labels.

## Asura copy

Copied into Asura (`rust/crates/asura-platform/tests/fixtures/secrets/`) on
2026-10-15 from Wisp commit `4305d2ceca370e2f8a8262bc916e36aa0b56086f`,
`training/secrets/`, for the secret detection tests (Asura design
`docs/designs/secret-detection.md`, decision SD8). Asura is MIT licensed; Wisp's
licence is in [`licenses/Wisp-MIT.txt`](licenses/Wisp-MIT.txt).

Further changes made in Asura:

- `train.tsv`, `dev.tsv` and `test.tsv` keep only comment lines and lines
  labelled `secret` or `none`. Every line labelled `personal` was removed. No
  kept line was edited; the U+200B marks remain. This removal is a further
  adaptation of the NVIDIA Nemotron-PII lines (CC BY 4.0) in `test.tsv`.
- `licenses/Wisp-MIT.txt` is Wisp's `LICENSE`.
- `wisp-rule-labels.tsv` is new: the label Wisp's `SecretScanner` rules give
  each kept line, for the parity measurement. It holds labels and rule kinds,
  no values.
- This section was added; the text above it is Wisp's `NOTICE.md` unchanged.

SHA-256 of each file, at the Wisp source and as copied:

| File | Wisp source | Asura copy |
| --- | --- | --- |
| `train.tsv` | `d4d5644cd3f57ff63354b04506f54add1f07c5f162523bf5f4326c42682a4933` | `22b04f610a61fd20ad4986d84d2e724a53adba1b3336e1383414e3ca7fb07750` |
| `dev.tsv` | `757af49827b1811bf21e6e6296b4eeefe2e4d9e2d23e2a07dfc90286c3a1f9ce` | `f27831d1a1a78036fe1f3e29bcafe81662fd22e41f420f8a4b1761f2af4c40c4` |
| `test.tsv` | `504890df832815a1c070a7c35f3042484149f08e7245c1129d28c48005065388` | `bed1b74b55d352ee2d889beb633fe53c490320ec83b646e9f3e52521d2776560` |
| `labels.md` | `601e0a2306d9fc004159c4208fa47cb65d791ecbdcdac67e50b09bc0404df12c` | `601e0a2306d9fc004159c4208fa47cb65d791ecbdcdac67e50b09bc0404df12c` |
| `NOTICE.md` (text above this section) | `7e1d82a402fae11d25bac3cd5214e4d4b41651d9c4d17a2148d4b805659d47dd` | identical |
| `licenses/Apache-2.0.txt` | `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30` | `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30` |
| `licenses/CC-BY-4.0.txt` | `9ba9550ad48438d0836ddab3da480b3b69ffa0aac7b7878b5a0039e7ab429411` | `9ba9550ad48438d0836ddab3da480b3b69ffa0aac7b7878b5a0039e7ab429411` |
| `licenses/CredSweeper-MIT.txt` | `694ee61a304a6e644734b5575f370f74a993b41c51c033ed8df95045f37b7c58` | `694ee61a304a6e644734b5575f370f74a993b41c51c033ed8df95045f37b7c58` |
| `licenses/gitleaks-MIT.txt` | `e3884b252b3bfc045e55be43a34d1e80da070bc6f804ac95bf4660e97d62ebc6` | `e3884b252b3bfc045e55be43a34d1e80da070bc6f804ac95bf4660e97d62ebc6` |
| `licenses/Privy-MIT.txt` | `c5604cf84fec10f8b48137ef8f3cd47e034b3d1533c94a6d5baf20d7a30fa785` | `c5604cf84fec10f8b48137ef8f3cd47e034b3d1533c94a6d5baf20d7a30fa785` |
| `licenses/Wisp-MIT.txt` | `440e656be7f04131c16dad3a15e0607ae8d47b3c30194e634851a90e23938832` (`LICENSE`) | `440e656be7f04131c16dad3a15e0607ae8d47b3c30194e634851a90e23938832` |
| `wisp-rule-labels.tsv` | generated | `978edaa063a13027dc82db567c612b25f37ce82339b00c37fed789feaafdeaec` |
