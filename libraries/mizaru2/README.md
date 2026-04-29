## What is Mizaru?

**Mizaru** is the anonymous authentication system used inside Geph5. It lets Geph verify that you have a valid account **without ever learning which server you're connecting to** — and without those servers learning who you are.

The core goal: Geph should be as private as an open, no-login proxy, even though it has accounts and payments.

---

## The problem it solves

Normally, authentication and privacy conflict. If a VPN server checks your credentials to let you in, it learns your identity. If Geph's login servers talk to your VPN server, they could track which servers you use and when.

Mizaru breaks that link. After you log in, it gives you a **cryptographic token** that proves you're a paying user — but that token is completely disconnected from your account. The VPN server can verify the token is legitimate without knowing anything about you. Even Geph's own login server cannot connect your token to you.

---

## Analogy: the sealed envelope

Imagine you want a librarian to stamp a note without reading it:

1. You write your note, seal it inside a special carbon-copy envelope, and hand it to the librarian.
2. The librarian stamps the outside of the envelope. The carbon transfers the stamp through to your note inside.
3. You open the envelope and take out your stamped note.

The librarian stamped the note but never saw its contents. Later, anyone can check the stamp is real — but they can't trace it back to you because the librarian never knew what was inside.

Mizaru works the same way. Your token is "sealed" with a **blinding factor** before it ever reaches Geph's authentication server. The server signs the blinded token. You remove the blinding factor and are left with a valid signature — one the server produced without ever seeing the actual token.

---

## The three parties

| Role | What they do | Trusted for anonymity? |
|------|-------------|------------------------|
| **Client** | You, the Geph user | — |
| **Broker** | Geph's login server — verifies your credentials and signs your blinded token | No |
| **Exit** | The VPN server you tunnel through | No |

Neither the broker nor the exit is trusted to protect your anonymity. The protocol is designed so that even if both are compromised or collude, they cannot link your account to your VPN usage.

---

## How it works, step by step

### 1. You generate a secret token

Your Geph client creates a random 32-byte value — your **anonymous identity token** for today. This token stays on your device and is never sent to anyone in its raw form.

### 2. You blind the token

Before sending anything to the broker, your client mathematically "blinds" the token using a random value called the **blinding factor**. The blinded version looks completely random — it carries no information about the original token.

### 3. The broker signs the blinded token

You send the blinded token to the broker along with your normal login credentials (username/password or subscription proof). The broker checks your credentials and, if valid, **signs the blinded token** using today's signing key. The broker never sees the unblinded token.

### 4. You unblind the signature

Your client removes the blinding factor from the signature. What remains is a valid cryptographic signature on your original token — one the broker produced without ever seeing it.

### 5. You connect to an exit

To connect to a VPN exit server, you present:
- Your original token
- The broker's signature on it
- A proof that today's signing key belongs to Geph (via a Merkle proof — see below)

The exit verifies the signature is genuine and that it's from today's valid key. It lets you in without knowing your account identity.

---

## Daily rotation and the key tree

To prevent tokens from one day being linked to tokens from another day, the broker uses a **different signing key for each calendar day**. There are 65,536 keys pre-generated, one for each day from the Unix epoch.

Rather than distributing all 65,536 keys, Geph publishes a single **Merkle root** — a short fingerprint that commits to the entire set of keys. When you need today's key, the broker sends it along with a **Merkle proof**: a compact chain of hashes that lets you verify the key is genuine without downloading all 65,536 keys.

This is important because it prevents a compromised broker from substituting a fake key that would let it trace your token.

The Merkle root is **hardcoded** in the Geph client, so it cannot be swapped out.

---

## Privacy properties

| Threat | Protection |
|--------|-----------|
| Broker tracking your VPN usage | Broker only sees a blinded token; it cannot connect that to any exit connection |
| Exit node learning your identity | Exit only sees a token and a signature; it has no link to your account |
| Two different days being linked | Each day uses a fresh token and a different signing key |
| Broker substituting a rogue key | Client verifies the key against the hardcoded Merkle root |
| Token replay | Tokens expire after 24 hours by design; within one day, some linkability is unavoidable (equivalent to a 24-hour burner account) |

---

## Cryptographic details (for technical readers)

<details>
<summary>Click to expand</summary>

Mizaru uses **RSA full-domain-hash (FDH) blind signatures** rather than elliptic-curve constructions. This is a deliberate choice: RSA-FDH blind signatures have a long, well-understood security history.

### Token and blinding

- Let $T \in \{0,1\}^{256}$ be the client token (32 random bytes).
- Let $(N_i, e_i)$ be today's RSA-2048 public key at index $i = \lfloor \text{UnixTime} / 86400 \rfloor \bmod 65536$.
- The client picks a random blinding factor $r$ coprime to $N_i$ and computes:

$$T' = \bigl(H(T) \cdot r^{e_i}\bigr) \bmod N_i$$

where $H$ is an FDH based on SHA-256.

### Signing and unblinding

- The broker computes $\sigma' = (T')^{d_i} \bmod N_i$ and returns it.
- The client recovers the unblinded signature: $\sigma = \sigma' \cdot r^{-1} \bmod N_i$.
- By RSA homomorphism, $\sigma = H(T)^{d_i} \bmod N_i$ — a valid FDH-RSA signature on $T$.

### Verification

The exit node checks:
1. **Merkle proof**: $(N_i, e_i)$ is the valid leaf at index $i$ under the hardcoded root.
2. **Signature**: $\sigma^{e_i} \bmod N_i \stackrel{?}{=} H(T)$.
3. **Epoch**: $i$ corresponds to the current 24-hour window.

### Key storage

The broker holds 65,536 RSA-2048 key pairs $\{(N_i, e_i, d_i)\}_{i=0}^{65535}$. The public keys are organized into a **blake3 Merkle tree**; the root is the broker's long-term public key.

</details>

---

## Security notes

- **Guard the broker's private keys.** A leaked epoch key allows forging valid tokens for that day.
- **The Merkle root must be correct.** It is hardcoded in client builds. A wrong root would allow a malicious broker to deanonymize users.
- **Tokens are reusable within 24 hours.** This is intentional — VPN sessions are long-lived, and per-request unlinkability is impractical. The privacy model is equivalent to users rotating to fresh burner accounts every day.
- **Forward privacy.** Each day the user gets a new, unlinked token. Past tokens cannot be connected to future ones.
