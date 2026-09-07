# Canonical JSON

**Protocol requirement.**

A signature covers bytes, not meaning. If a Rust node and a browser client
serialize the same logical message differently, they compute different
hashes, fail to verify each other's signatures, and quietly stop being the
same network. Canonicalization is what stops that.

c0mpute uses **RFC 8785 (JCS)** with two restrictions that remove its
hardest corners. Both are *checked*, not assumed: a payload that violates
one fails to canonicalize, so the error lands at signing time rather than
on someone else's verification.

## The rules

1. **Objects**: keys sorted, ascending, by their raw bytes.
2. **Arrays**: order preserved.
3. **No insignificant whitespace** anywhere.
4. **Strings**: `"` and `\` escaped; `\b \f \n \r \t` use their short
   forms; other characters below `U+0020` use lowercase `\u00xx`;
   everything else is emitted as literal UTF-8.
5. **No floating-point numbers.** *(restriction)*
6. **Object keys must be ASCII.** *(restriction)*
7. Integers must satisfy `|n| ≤ 2^53 − 1`.

## Why no floats

JCS canonicalizes doubles through the ECMAScript `Number::toString`
algorithm. It is well-specified and genuinely hard to reimplement exactly,
in every client language, in the one place where a bug is unrecoverable:
once a wrong hash has been signed and gossiped, there is no fixing it
after the fact.

The cost of avoiding it is one string field:

```json
"price": { "amount": "0.042", "currency": "USD" }
```

`0.1 + 0.2 != 0.3` in binary floating point, and encoders disagree about
how to print a double. Money as a decimal string sidesteps both. Durations
and sizes are integers. Nothing in a signed c0mpute payload is a float,
and the canonicalizer rejects one rather than guessing.

The `2^53 − 1` bound exists for the same reason from the other direction:
a JSON parser backed by JavaScript numbers silently loses precision above
it. Anything larger belongs in a string.

## Why ASCII keys

JCS sorts object keys by UTF-16 code unit. Above `U+FFFF` that ordering
diverges from UTF-8 byte order, so an implementation that sorts raw bytes
— the obvious thing to do — is subtly wrong for a narrow range of keys it
will probably never see, which is the worst kind of bug to own.

Restricting *keys* to ASCII makes the two orderings identical. **Values
are unrestricted UTF-8**: `{"note":"café ☕"}` is fine, `{"café":1}` is
not. Wire field names are all camelCase ASCII anyway.

## Worked example

Input, with keys in an arbitrary order and a nested object:

```json
{
  "z": { "second": 2, "first": 1 },
  "a": [ { "y": 1, "x": 2 } ]
}
```

Canonical form:

```json
{"a":[{"x":2,"y":1}],"z":{"first":1,"second":2}}
```

Note that the *array* keeps its order while the objects inside it are
sorted.

## What this buys

Because canonicalization is deterministic, a content hash is a stable name
for a record. Two nodes that received the same envelope compute the same
hash, so an offer can cite a job by hash, a receipt can cite an offer, and
none of them has to re-send the thing they are citing or be trusted about
its contents.

It also means a record survives a round trip through storage. Serialize
canonically, read back, re-hash: same answer. That is what lets a buyer
rebuild local job state from an on-disk event log after a restart, with no
database and no server to ask.

## Reference

`node/crates/c0mpute-envelope/src/canonical.rs`. The rules above are each
covered by a test in that file.
