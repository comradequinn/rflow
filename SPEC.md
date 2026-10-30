# hflow specification

This document defines what hflow does and how it is organised. It is written to be language-neutral: an implementation in any language that meets it is a faithful rebuild of hflow. It does not prescribe libraries, data types or code structure beyond the components and contracts described here.

## Naming

`hflow` is the name of the specification, not of any implementation. Each implementation replaces the `h` with a prefix for its language or stack, for example `rflow` for Rust, `gflow` for Go and `cflow` for C#.

Wherever `hflow` appears below, an implementation uses its own name instead. This covers the executable name, the usage text, the data directory, the CA's name, the `hflow:` prefix on error messages and responses, the start-up message, the exported CA file name in the README and `.gitignore`, and the repository name. For Rust, for example, these are `rflow`, `~/.rflow`, `rflow CA`, `rflow: …` and `rflow-ca.pem`.

## 1. Purpose

hflow is a command-line HTTP and HTTPS proxy for inspecting and changing traffic from a terminal.

- It is a single executable with no runtime dependencies: no daemon, no config file, no browser UI.
- It prints requests and/or responses as they pass through.
- It lets the user edit requests before they are sent, or responses before they reach the client, in a text editor.
- It intercepts HTTPS by acting as its own certificate authority (CA) and issuing certificates per host on demand, caching them on disk.

**Guiding principle:** add nothing that is not specifically needed. Anything not described here is out of scope (see §11).

## 2. Command-line interface

```
usage: hflow [options]

options:
  --port <port>        port to listen on (default 8080)
  --print-req          print requests to stdout
  --print-resp         print responses to stdout
  --edit-req <text>    edit requests whose URL contains <text> before sending
  --edit-resp <text>   edit responses to requests whose URL contains <text>
  --ca                 write the CA certificate to stdout and exit
  -h, --help           show this help

edits open in $EDITOR, or vim if it is unset
```

Rules:

- Options are long-form with a double dash. `-h` is the only short form.
- Options may appear in any order and any combination. If an option is repeated, the last one wins.
- `--port` takes an integer from 0 to 65535.
- `--edit-req` and `--edit-resp` are independent. Each takes its own text, so one run may edit requests to one site and responses from another.
- With no options, hflow runs as a plain pass-through proxy on port 8080.

Exit behaviour:

| Situation | Output (stderr) | Exit code |
|---|---|---|
| Unknown option, missing value or invalid port | the error (e.g. `unknown argument: -x`, `--port requires a value`, `invalid port: abc`), a blank line, then the usage text | 2 |
| `-h` / `--help` | the usage text | 2 |
| `--ca` | the CA certificate in PEM format on **stdout** | 0 |
| Failure at start-up or while running (e.g. port in use, `HOME` not set) | `hflow: <error>` | 1 |

## 3. Data directory

All state lives in `~/.hflow` (`$HOME/.hflow`), which is created if it is missing.

| Path | Contents | Permissions |
|---|---|---|
| `ca.key` | CA private key (PEM) | owner read/write only |
| `ca.crt` | CA certificate (PEM) | default |
| `certs/<host>.pem` | a host's certificate followed by its private key (PEM) | owner read/write only |
| `edit.http` | the message currently being edited; exists only while an edit is open | owner read/write only |

`<host>` is the host name with every character other than ASCII letters, digits, `.` and `-` replaced by `_`.

## 4. Architecture

hflow is split into the components below. Each has one concern, and they meet only through the contracts described. This is what makes it extensible: **new behaviour is added as a new interceptor plus a flag, with no change to the proxy, the chain or the other interceptors.**

```
            ┌────────────────────── Composition root (CLI) ──────────────────────┐
            │ parses options, loads the CA, builds the chain, starts the proxy   │
            └────────────────────────────────────────────────────────────────────┘
                                   │                      │
                                   ▼                      ▼
client ──▶ Proxy (transport) ──▶ Chain ──▶ Interceptor 1 … N         Certificate authority
                 │                  ▲                                    ▲
                 └──── upstream ────┘                                    │
                 └───────────────── per-host TLS identity ───────────────┘
```

| Component | Responsibility | Knows nothing about |
|---|---|---|
| **Message model** | Plain, fully buffered representations of a request and a response | Networking, TLS, the terminal |
| **Interceptor contract** | The two hooks every interceptor may implement | Which interceptors exist |
| **Chain** | Holds an ordered list of interceptors and runs messages through them | What any interceptor does; networking |
| **Interceptors** | One behaviour each (print request, print response, edit request, edit response) | Each other; networking; TLS |
| **Proxy (transport)** | Accepting clients, tunnelling and terminating TLS, converting wire messages to and from the model, sending upstream, error responses | What the interceptors do |
| **Certificate authority** | Creating/loading the CA, issuing and caching per-host certificates | HTTP, interceptors |
| **Terminal arbitration** | Gives one interceptor at a time exclusive use of the terminal | Messages |
| **Editor** | Shows bytes to the user and returns edited bytes, or "cancelled" | HTTP, file formats |
| **Composition root** | Turns options into a configured chain and proxy | Message handling |

### 4.1 Message model

A **request** has: method, URI, protocol version, headers and body.

- The URI is always absolute, e.g. `https://example.com/path?query`, whether the request arrived as plain HTTP or inside an HTTPS tunnel.
- Headers are an ordered multimap: a name may occur more than once. Names keep the order in which they first appeared, and multiple values for one name stay together in their original order.
- The body is a complete byte sequence, possibly empty. Bodies are always fully buffered before the chain runs. There is no streaming.

A **response** has: status code, protocol version, headers and body (same rules as above).

### 4.2 Interceptor contract

An interceptor may implement either or both hooks. A hook that is not implemented does nothing and succeeds.

- **request hook**: receives the request and may change any part of it, or replace it entirely. It may fail with a reason, which **drops** the request.
- **response hook**: receives the original request (read-only, as it was after the request hooks ran) and the response. It may change or replace the response, or fail with a reason, which **drops** the response.

Interceptors must be safe to call from many connections at once.

### 4.3 Chain

- Interceptors are added in a fixed order when hflow starts and never change while it runs.
- **Requests** pass through the interceptors in the order they were added.
- **Responses** pass back through them in **reverse** order.
- The chain stops at the first hook that fails and reports that failure. Later interceptors do not see the message.

### 4.4 Composition root

The composition root builds the chain from the options in exactly this order:

1. Edit request (if `--edit-req`)
2. Print request (if `--print-req`)
3. Print response (if `--print-resp`)
4. Edit response (if `--edit-resp`)

Because responses run in reverse, the edits sit at the outer ends of the chain. As a result, printing always shows the message after editing: the request as it was actually sent upstream, and the response as the client actually received it.

The composition root also, in this order:

1. Loads or creates the CA (§7). This happens on every run, including `--ca`.
2. If `--ca` was given, writes the CA certificate to stdout and exits.
3. Starts the proxy on `127.0.0.1:<port>`.

## 5. Proxy behaviour

### 5.1 Listening

- hflow listens on `127.0.0.1` only, on the configured port.
- When ready, it writes `hflow listening on 127.0.0.1:<port>` to stderr.
- It speaks HTTP/1.1 (and 1.0) to clients. Each connection is handled concurrently. Connections may carry multiple requests (keep-alive).

### 5.2 Plain HTTP requests

A request whose target is an absolute URI (e.g. `GET http://host/path HTTP/1.1`) is handled by the exchange in §5.4.

A non-CONNECT request whose target is not absolute gets `400`, body `hflow: request URI must be absolute\n`.

### 5.3 HTTPS (CONNECT) requests

1. If the CONNECT target has no `host:port`, reply `400`, body `hflow: CONNECT requires host:port\n`.
2. Get a TLS identity for the host from the CA (§7). Square brackets around IPv6 addresses are removed first. If this fails, reply `500`, body `hflow: certificate error\n`, and log `certificate error for <host>: <error>` to stderr.
3. Otherwise reply `200` with an empty body, then act as the TLS server on the tunnelled connection, presenting the host's certificate and offering only `http/1.1` via ALPN.
4. Serve HTTP/1.1 inside the tunnel. Each request there arrives in origin form (`/path?query`); rebuild its URI as `https://<host>[:<port>]<path>`, leaving out the port when it is 443. A request whose rebuilt URI is invalid gets `400`, body `hflow: invalid URI\n`.
5. Handle each rebuilt request with the exchange in §5.4.
6. If the tunnel or TLS handshake fails, log `tunnel error for <host:port>: <error>` to stderr.

### 5.4 The exchange

For each request:

1. Read the entire body and build the request model.
2. Remove hop-by-hop headers (§5.5).
3. Run the request through the chain. If it is dropped: log `dropped <METHOD> <URI>: <reason>` to stderr and reply `502`, body `hflow: request dropped: <reason>\n`. Stop.
4. Send the (possibly changed) method, URI, headers and body upstream, over HTTP/1.1 for `http` URIs and HTTP/1.1 over TLS for `https` URIs. Upstream servers' certificates are verified against a built-in set of public web root CAs (Mozilla's), not the system trust store.
5. Read the entire response body and build the response model.
6. Remove hop-by-hop headers (§5.5).
7. Run the response through the chain, passing the request as it was sent. If it is dropped: log `dropped response to <METHOD> <URI>: <reason>` to stderr and reply `502`, body `hflow: response dropped: <reason>\n`. Stop.
8. Reply to the client with the (possibly changed) status, headers and body. The transport sets the framing for the body (e.g. a correct `Content-Length`).

If anything in steps 1, 4 or 5 fails (e.g. DNS failure, refused connection, invalid upstream certificate), log `upstream error: <error>` to stderr and reply `502`, body `hflow: <error>\n`.

All error responses generated by hflow have a body of the form `hflow: <message>\n` and no other headers that the client depends on.

### 5.5 Hop-by-hop headers

Removed from both requests and responses before the chain sees them:

- every header named in a `Connection` header's comma-separated value;
- `Connection`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`, `Keep-Alive`, `Proxy-Connection`.

Header names are case-insensitive and are presented to interceptors in lower case.

## 6. Interceptors

### 6.1 Terminal arbitration

Printing and editing both use the terminal. Every print and every edit takes exclusive use of the terminal for its duration, so:

- output from different messages never interleaves;
- nothing is printed over an open editor;
- only one edit is open at a time. Other messages that need printing or editing wait until it is finished. Messages that need neither are not held up.

### 6.2 Print request (`--print-req`)

Writes the request to stdout, then an extra blank line. It never changes the request and never fails.

```
> <METHOD> <URI> <VERSION>
<name>: <value>
…
                                  ← blank line, only if the body is not empty
<body>                            ← only if the body is not empty
```

- `<VERSION>` is written as `HTTP/1.0`, `HTTP/1.1` or `HTTP/2.0`.
- One line per header value, in order, as `name: value`.
- If the body is not empty, a blank line follows the headers, then the body, then a newline.
- If the body is valid UTF-8 it is written as text; otherwise it is written as `[<N> bytes of binary data]`.

Example:

```
> POST http://example.com/login HTTP/1.1
content-type: text/plain

user=bob
```

### 6.3 Print response (`--print-resp`)

The same as §6.2, with this first line, which also names the request it answers:

```
< <VERSION> <CODE> <REASON> (<METHOD> <URI>)
```

`<REASON>` is the standard reason phrase for the code. Example:

```
< HTTP/1.1 404 Not Found (GET http://example.com/data)
content-type: text/plain

not here
```

### 6.4 Edit request (`--edit-req <text>`)

Applies only when the request's full URI contains `<text>` as a plain, case-sensitive substring, e.g. `example.co` matches both `www.example.com` and `home.example.co.uk`. Other requests pass through untouched.

1. Serialise the request in the edit format (§6.6) with the start line `<METHOD> <URI> <VERSION>`.
2. Open it in the editor (§6.7).
3. If the edit was cancelled, the request continues unchanged.
4. Otherwise parse the result (§6.6) and replace the whole request with it, so headers removed in the editor are removed from the request.
5. If parsing fails, the request is dropped with the parse error as the reason, so the client gets a `502` that gives the reason.

### 6.5 Edit response (`--edit-resp <text>`)

Applies only to responses whose **request** URI contains `<text>`. Otherwise as §6.4, with the start line `<VERSION> <CODE> <REASON>`.

### 6.6 Edit format

Serialising a message:

```
<start line>\n
<name>: <value>\n        ← one per header value, in order
\n
<body>                   ← raw bytes, nothing appended
```

Parsing the edited file:

1. Split at the first `\n\n`. Before it is the head; everything after it, byte for byte, is the body. If there is no `\n\n`, the whole file is the head and the body is empty.
2. The head must be UTF-8. Its first line is the start line; each later line is a header.
3. Each header line is split at its **first** colon into name and value, both trimmed of surrounding whitespace (so `host: example.com:8080` keeps its port). A line with no colon fails with `invalid header: <line>`. Invalid header names or values fail.
4. Any `Content-Length` header is removed, so the length is recalculated from the new body when it is sent.
5. A **request** start line must be exactly three whitespace-separated parts, `METHOD URI VERSION`, or it fails with `first line must be: METHOD URI VERSION`. The URI must be absolute (scheme and host), or it fails with `URI must be absolute`. The method must be a valid HTTP method token.
6. A **response** start line must have at least two whitespace-separated parts, `VERSION STATUS`, or it fails with `first line must be: VERSION STATUS`. The status must be a three-digit number from 100 to 999. Anything after the status (the reason phrase) is ignored.
7. The version must be `HTTP/1.0`, `HTTP/1.1` or `HTTP/2.0`, or it fails with `unsupported version: <version>`.

Changing the host in the URI does not change the `host` header. The user must do that themselves.

### 6.7 Editor

The editor is an abstraction with one operation: given bytes, show them to the user and return either the edited bytes or **cancelled**. The edit interceptors depend only on this abstraction, so tests can substitute a fake editor that returns fixed text or "cancelled" without a terminal.

The real editor:

1. Writes the bytes to `~/.hflow/edit.http` (owner read/write only).
2. Runs the command in `$EDITOR` with the file path as its last argument, attached to the user's terminal, and waits for it to exit.
   - `$EDITOR` is split on whitespace into a program and arguments, so `code --wait` works.
   - If `$EDITOR` is unset, empty or only whitespace, `vim` is used.
3. Reads the file back, then deletes it.
4. If the editor exited with a non-zero status (e.g. `:cq` in vim), the result is **cancelled**.
5. If the editor could not be started, or the file could not be read back after a successful exit, the edit fails and the message is dropped.

**Trailing newline rule.** Most editors add a newline to the end of the file when saving. After any edit that was not cancelled: if the original serialised message did **not** end with `\n` and the edited result **does**, remove exactly one trailing `\n`. Otherwise leave the result as it is. This applies to all editors, not only vim.

## 7. Certificate authority

### 7.1 The CA

- On start, if both `ca.key` and `ca.crt` exist, they are loaded. Otherwise a new key pair and self-signed certificate are generated and saved, replacing either file if only one existed.
- The CA certificate has common name and organisation `hflow CA`, is marked as a CA with no path-length limit, and has the key usages *certificate signing* and *CRL signing*.
- Keys are ECDSA P-256, signed with SHA-256.
- `--ca` writes the contents of `ca.crt` to stdout unchanged.

Anyone with `ca.key` can intercept traffic from any client that trusts the CA, which is why it is readable only by its owner.

### 7.2 Host certificates

When a CONNECT for a host arrives, hflow needs a TLS identity for that host:

1. **In-memory cache.** If one was already built for this host during this run, use it.
2. **Disk cache.** Otherwise, if `certs/<host>.pem` exists and was last modified less than 300 days ago, load it.
3. **Issue.** Otherwise generate a new one and write it to `certs/<host>.pem`, replacing any old one:
   - a new ECDSA P-256 key pair;
   - subject common name: the host;
   - subject alternative name: the host (as an IP address if it is one, otherwise as a DNS name);
   - extended key usage: server authentication;
   - valid from one day before now until 365 days after now;
   - signed by the CA.
4. Add it to the in-memory cache.

Reissuing at 300 days keeps cached certificates well clear of their 365-day expiry.

## 8. Concurrency

- Client connections, and requests within them, are handled concurrently.
- While an edit is open, only messages waiting to print or edit are held up (§6.1). Other traffic keeps flowing.
- The certificate caches are safe to use from many connections at once. Two simultaneous first connections to the same host may both issue a certificate. Either result is acceptable.

## 9. Repository

- `README.md`: a short introduction (single binary, no runtime dependencies, simple, low latency) followed by worked examples: trusting the CA, printing requests, printing both directions, changing the port, editing requests, editing responses, editing one site's requests and another's responses, combining editing with printing, and proxying a whole shell session via `http_proxy`/`https_proxy`.
- `.gitignore`: the build output directory and `hflow-ca.pem` (the file the README tells users to export the CA to).
- Remote: `github.com/comradequinn/<name>`, public, branch `main`, where `<name>` is the implementation's name, e.g. `github.com/comradequinn/rflow`.

## 10. Tests

Unit tests must be simple enough to read at a glance, named as sentences that state what they verify, and must not need a network, terminal or real editor. They cover at least:

**Chain**
- requests pass through interceptors in the order added;
- responses pass through in reverse order;
- a dropped request stops the chain (earlier interceptors ran, later ones did not);
- a dropped response stops the chain in the same way.

**Print request / print response**
- exact output for a request with headers and a body;
- no body section when the body is empty;
- a binary body is summarised as `[N bytes of binary data]`;
- exact output for a response, including the request it answers;
- printing does not change the message.

**Edit request / edit response** (using a fake editor)
- messages whose URL does not match are not opened in the editor;
- the pattern matches any part of the URL;
- the editor is shown exactly the serialised message;
- a saved edit replaces the whole message (method, URI, headers, body / status, headers, body);
- a cancelled edit keeps the original;
- an invalid start line drops the message with the exact error;
- a relative URI drops the request;
- a header line with no colon drops the message with the exact error;
- a non-numeric status drops the response;
- `Content-Length` is removed;
- colons in header values are kept;
- blank lines in the body are kept;
- a trailing newline added by the editor is removed when the original had none;
- a trailing newline is kept when the original already had one.

## 11. Out of scope

These are deliberately not supported. Adding any of them is a change to this specification.

- Listening on any interface other than `127.0.0.1`.
- HTTP/2 or HTTP/3 to clients or upstream, WebSockets, or other protocol upgrades (the `Upgrade` header is stripped).
- Streaming bodies; every body is buffered in full.
- Decompressing bodies for printing or editing. Compressed bodies print as binary and edit as unreadable text.
- Matching by anything other than a case-sensitive URL substring (no regular expressions, methods or headers).
- Proxy authentication, upstream proxies, a config file, logging to file, or a UI.
- Using the system trust store for upstream verification, or accepting invalid upstream certificates.
- Automatically updating the `host` header when an edited URI changes host.
- Platforms without POSIX file permissions (the private files in §3 rely on them).
