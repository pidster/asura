# Technical writing standard

Status: required practice for repository documentation.

## Purpose and basis

Write so that a repo owner or implementation agent can identify the actor, action
and required result on the first reading. Keep technical distinctions that affect
correctness, security or recovery.

This standard adopts selected practices from
[ASD-STE-100 Issue 9](https://www.asd-ste100.org/assets/files/ASD-STE100_ISSUE9.pdf).
It is a project writing standard, not a claim of full STE compliance. Asura keeps
its technical vocabulary and existing spelling conventions. A full compliance
check would also require the STE dictionary and its terminology rules.

## Sentences and terms

1. Name the actor. Prefer “The orchestrator records the result” to “The result is
   recorded.” Use an imperative only when instructing the document's reader.
2. Give one instruction per sentence. Give one main topic per paragraph.
3. Aim for at most 20 words in a procedural sentence and 25 in a descriptive
   sentence. These are editing targets, not automatic acceptance tests.
4. Keep a longer sentence when splitting it would obscure an atomic operation,
   condition or exception. Explain that relationship in the surrounding text.
5. Put a condition before the action it controls. State the failure result as
   well as the successful result.
6. Use the same term for the same concept. Define project terms in the
   [glossary](glossary.md), then link to the detailed contract where needed.
7. Prefer verbs to noun clusters. Write “reserve the task budget” instead of
   “perform aggregate task budget reservation.”
8. Explain necessary technical terms before combining them. Keep identifiers,
   state names and API names exact.
9. State limits precisely. If a value remains undecided, identify the design stage
   that must choose it. “Bounded” alone does not specify a limit.
10. Remove repeated qualifications and vague praise. Retain concrete prohibitions
    such as “Do not retry an operation with an unknown outcome.”

## Status and requirements

Use a short status block at the start of each design. State what is fixed and
what remains open. Use these labels within documents that mix different statuses:

| Label | Meaning |
| --- | --- |
| Required behavior | A user requirement or selected decision that later designs must preserve |
| Proposed mechanism | A possible way to meet a requirement; it is not yet selected |
| Open decision | A question that the named design stage must resolve |
| Verified behavior | A result supported by recorded checks in a named environment |

Use **must** for requirements, **may** for permitted choices, and **propose** for
unselected approaches. Avoid “should” when the reader needs a definite obligation.
Present tense in a required-behavior section describes the intended system. It
does not imply that the system is implemented.

Separate the requirement from its rationale and verification method. Keep each
rule in one governing document. Link to it from plans and ADRs.

## Tables and acceptance cases

Use tables for short comparisons and indexes. Do not put a procedure or several
test scenarios in one cell. Give detailed acceptance cases stable IDs and use:

- **Initial state:** relevant task, data, permissions and running operations.
- **Trigger:** the event or fault under test.
- **Required result:** observable behavior, including forbidden effects.
- **Unit, integration and end-to-end checks:** the evidence needed at each layer.
- **Environment:** real services, hosts or models needed to support the claim.

Keep independent race and fault cases identifiable. A summary case may group them,
but the detailed test specification must assign each combination its own test ID.

## Diagrams and review

Start with a small overview. Link it to detailed state, sequence and failure views.
Keep labels short and use the same terms as the prose. Split diagrams when a
reader must repeatedly zoom or follow long crossing paths to understand one case.
Each view must state its scope and the meaning of its arrows.

Apply the [diagram checks](design-process.md#diagram-validation), including visual
inspection at a normal document width. Syntax success alone is insufficient.

During an editorial change, compare the old and new text for lost conditions,
limits, owners and test cases. Document any intended change to behavior separately.
Writing rules must not remove a security condition to meet a word-count target.

## Directory READMEs

Every documentation directory must contain a `README.md` that explains its purpose
and indexes its immediate files and child directories. Exclude the index itself.
Give each entry a relative link and a concise description. Preserve distinctions
between requirements, proposals, selected designs and recorded evidence.

Update the affected indexes whenever a document is added, renamed, moved or
removed. A new documentation directory needs its README in the same change.
Check link targets and index completeness before delivery. Keep detailed contracts
in their canonical files; an index explains where to read rather than copying them.

## Adoption decision

The 2026-09-20 clarity review found unexplained terms, overloaded acceptance tables
and diagrams that were difficult to read. This standard addresses those problems.
It uses STE as guidance while retaining the technical terms needed for software
design. The design process owns implementation approval; this standard owns writing.
