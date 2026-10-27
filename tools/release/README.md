# Release packaging

`asura-bundle` builds, signs and checks `Asura.app`. The
[release signing design](../../docs/designs/release-signing.md#sg1-package-script)
governs it. Only the owner signs with a real identity and notarises, on the
owner's Mac (SGQ9). No test and no CI job holds an identity or a notary
profile.

| File | Purpose |
| --- | --- |
| `asura-bundle` | The package script: `--ad-hoc` (dry run), `--development` or `--release` |
| `test-asura-bundle` | Checks: `--static` (tracked files and refusals), `--bundle APP` (an ad hoc bundle), `--sandbox APP` (the helper App Sandbox with an ad hoc bundle, SG-V5) |
| `sandbox-probe.c` | The SG-V5 negative fixture that `--sandbox` builds and signs with the helper's sandbox list |
| `secret-store-qualification` | The signed qualification of the team Keychain store (SG3, SGQ6): builds, signs and runs `AsuraKeychainQualification.app` with the test App ID |
| `Info.plist.in` | `Asura.app/Contents/Info.plist`; `@VERSION@` is the `asura-cli` version |
| `entitlements/*.plist` | One list for each executable and mode; `@TEAM_ID@` is replaced at signing time |

## Before the first run

1. Use the verified generators. Both paths must be absolute:

   ```sh
   export ASURA_PROTOC="$PWD/.build/dev-tools/protoc"
   export ASURA_PROTOC_GEN_SWIFT="$PWD/.build/model-tools/swift-build/out/Products/Release/protoc-gen-swift"
   ```

2. Put the profiles in the ignored directory `.build/signing/`. Never commit
   a profile, a `.p12` or a `.p8` file.
3. For a release, store the notary credential once, then delete the `.p8`
   file:

   ```sh
   xcrun notarytool store-credentials <notary-profile> \
     --key <AuthKey_KEYID.p8> --key-id <KEYID> --issuer <ISSUER-UUID>
   ```

4. Create the parent of the output directory (`mkdir -p .build/bundles`). The
   output directory itself must not exist.

## Commands

Dry run, ad hoc signed. It needs no identity and touches no keychain:

```sh
tools/release/asura-bundle --ad-hoc --out .build/bundles/dry-run-1
tools/release/test-asura-bundle --static
tools/release/test-asura-bundle --bundle .build/bundles/dry-run-1/Asura.app
```

Development bundle (Apple Development identity, development profile, SGQ3):

```sh
tools/release/asura-bundle --development --out .build/bundles/dev-1 \
  --identity "Apple Development: <Name> (<CERT-ID>)" \
  --team-id <TEAMID> \
  --profile .build/signing/<development>.provisionprofile
```

Release (Developer ID identity, Developer ID profile, notarised and stapled):

```sh
git status --short   # must print nothing
tools/release/asura-bundle --release --out .build/bundles/release-0.1.0 \
  --identity "Developer ID Application: <Name> (<TEAMID>)" \
  --team-id <TEAMID> \
  --profile .build/signing/<developer-id>.provisionprofile \
  --notary-profile <notary-profile>
```

The identity inputs may come from `ASURA_SIGN_IDENTITY`, `ASURA_TEAM_ID`,
`ASURA_PROFILE` and `ASURA_NOTARY_PROFILE` instead; an argument wins. The
script stores none of them in a tracked file and prints only the identity
name, the team ID and the certificate chain names. The ad hoc mode ignores
these variables and refuses the arguments.

`notarytool submit --wait` has no time bound. Ctrl-C cancels the wait; the
submission continues at Apple. Query it later with
`xcrun notarytool info <submission-id> --keychain-profile <notary-profile>`.
Do not submit the same build again. Stapling retries at 1, 2, 4 … 60 s until it
succeeds or you press Ctrl-C.

## Outputs

| Path in `--out` | Contents |
| --- | --- |
| `Asura.app` | The signed bundle |
| `package/` | The signed helper, `mlx.metallib`, the Hub bundle and `model-package.json`, in the development package layout |
| `asura-<version>-macos-arm64.tar.gz` and `.sha256` | Release archive (`Asura.app` and `LICENSE`). The ad hoc run names it `…-adhoc-unstapled.tar.gz`. A development run writes none |
| `notary-submit.plist`, `notary-log.json` | Release only: the notary reply, and its log when not accepted |

The script also rewrites the ignored generated Swift sources and
`.build/model-tools/package-identity.txt`. Later Cargo builds embed this
package's identity. To run native tests against it, place the files from
`package/` beside the test binaries (helper README, step 5).

## Helper App Sandbox and end-to-end check of the bundle layout

Every `asura-model` list claims the App Sandbox, `network.client` and
read-only access to `~/.asura/data/models/` and `~/.asura/run/` of the account
home (SG2). Tests never use that home, so the bundle's own helper cannot read a
test home. `--sandbox` therefore re-signs a copy of the bundle's helper as a
test-only variant: the shipped list plus a read-only exception for
`/private/tmp/`, under the probe identifier `org.pidster.asura.probe.model`.
No tracked list carries that exception. It then:

1. runs `sandbox-probe` with the same identifier and list: `~/.ssh` reads,
   writes to the account home and to `/private/tmp/` are denied; reads of
   `/private/tmp/` and writes in the container are allowed; an unsandboxed
   control can do each of them;
2. writes the variant's digest to `.build/model-tools/package-identity.txt`,
   builds `conversation_flow` and runs `--bundle-sandbox`: the service runs
   from the variant's `Asura.app` layout, every helper must be sandboxed, and
   the inventory, a system turn, an MLX turn (when `ASURA_TEST_MLX_ASSET` is
   set) and an Ollama turn (when a local Ollama already serves
   `ASURA_TEST_OLLAMA_MODEL`, default `granite4.1:8b`) complete, then a
   service crash and recovery;
3. restores the package identity file, removes its work directory and the
   `Data` directory of the probe container, and lists what the container
   keeps.

```sh
ASURA_TEST_MLX_ASSET=<path to an MLX model directory> \
tools/release/test-asura-bundle --sandbox .build/bundles/dry-run-1/Asura.app
```

The script never starts the bundle's own helper: the package script checks
`--version` on an unsandboxed ad hoc copy, so that packaging creates no
container under `org.pidster.asura.model`.

## Keychain store qualification

`secret-store-qualification` proves the team Keychain store on the real
data-protection keychain
([signed qualification run](../../docs/designs/release-signing.md#signed-qualification-run)).
It builds the `test-support` binary `secret_store_qualification` into
`<out>/cargo-target`, assembles `AsuraKeychainQualification.app`
(`CFBundleIdentifier` `org.pidster.asura.test`), signs it with the
Hardened Runtime and runs it. The binary drives the production store worker
over `Target::Default`. It writes items only in
`<TeamID>.org.pidster.asura.test`, purges that group before and after the
cases, and passes only when it ends with `test-group-empty=yes`.

Signed run, by the owner only (SGQ6, SGQ9). It needs a profile for the test
App ID `org.pidster.asura.test` (development or Developer ID) whose
`keychain-access-groups` allows the test group or `<TeamID>.*`:

```sh
ASURA_PROTOC="$PWD/.build/dev-tools/protoc" \
tools/release/secret-store-qualification --signed --out .build/bundles/kc-qual-1 \
  --identity "Developer ID Application: <name> (<TeamID>)" --team-id <TeamID> \
  --profile <path to the org.pidster.asura.test Developer ID profile>
```

An Apple Development identity with the test App ID's development profile
works the same way. Before and after, `security list-keychains -d user`
(read only) must list only the login keychain. If a run is cancelled, the
next run purges the test group first.

Ad hoc check (no identity; `test-asura-bundle` does not run it because it
builds in release mode): the bundle is signed without restricted
entitlements, and the executable must refuse with no Keychain call:

```sh
ASURA_PROTOC="$PWD/.build/dev-tools/protoc" \
tools/release/secret-store-qualification --ad-hoc --out .build/bundles/kc-qual-adhoc-1
```

## What the dry run does not prove

- The restricted entitlements (`keychain-access-groups`, application and team
  identifiers). An ad hoc process that claims them is killed at launch, so the
  ad hoc lists omit them. Probes KC-P1 and KC-P2 need a signed bundle.
- Notarisation and stapling (probe NT-P1, packet SG4), including whether the
  notary service accepts the helper's temporary exceptions.
- The helper App Sandbox under a real signature. The ad hoc helper claims the
  same list, and `--sandbox` proves it with a test variant.
