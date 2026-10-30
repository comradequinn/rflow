# rflow

rflow is a simple, low-latency HTTP and HTTPS proxy for inspecting and changing traffic from the command line. It is a single binary with no runtime dependencies: no daemon, no config file and no browser UI. Point a client at it, choose what to do with the traffic using flags, and watch it in your terminal.

- **Print** requests and responses as they pass through.
- **Edit** requests before they are sent, or responses before they reach the client, in your editor.
- **HTTPS** traffic is intercepted using certificates that rflow generates and caches automatically.

## Install

```bash
cargo install --path .
```

## Usage

```
usage: rflow [options]

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

rflow listens on `127.0.0.1`. The CA, the host certificates it generates and the temporary edit file are kept in `~/.rflow`.

## Examples

### Trust the CA for HTTPS

On first run, rflow creates its own certificate authority. Export it so clients can trust it:

```bash
rflow --ca > rflow-ca.pem
```

Then either pass it to each client, as in the examples below, or add it to your system's trust store. On Fedora:

```bash
sudo cp rflow-ca.pem /etc/pki/ca-trust/source/anchors/
sudo update-ca-trust
```

Anyone who has the CA's private key can intercept traffic from any client that trusts it, so keep `~/.rflow` private.

### Print requests

```bash
rflow --print-req
```

In another terminal:

```bash
curl -x http://localhost:8080 http://httpbin.org/get
curl -x http://localhost:8080 --cacert rflow-ca.pem https://httpbin.org/get
```

### Print requests and responses

```bash
rflow --print-req --print-resp
```

```bash
curl -x http://localhost:8080 --cacert rflow-ca.pem \
  -d 'name=bob' https://httpbin.org/post
```

Requests are marked `>` and responses `<`. Each response line also names the request it answers.

### Listen on a different port

```bash
rflow --port 9000 --print-req
curl -x http://localhost:9000 http://httpbin.org/get
```

### Edit a request before it is sent

Hold any request whose URL contains `httpbin.org` and open it in your editor:

```bash
rflow --edit-req httpbin.org
```

```bash
curl -x http://localhost:8080 --cacert rflow-ca.pem 'https://httpbin.org/anything?foo=bar'
```

The editor opens with the full request:

```
GET https://httpbin.org/anything?foo=bar HTTP/1.1
host: httpbin.org
user-agent: curl/8.11.1
accept: */*

```

Change the method, URL, headers or body, then save and close the editor. httpbin.org/anything echoes back what it receives, so curl shows your edited request. `Content-Length` is recalculated for you.

- Quitting the editor with an error, `:cq` in vim, sends the original request unchanged.
- If the edited file can't be parsed, the request is dropped and the client gets a `502` that gives the reason.
- The match is on any part of the URL, so `--edit-req example.co` matches `www.example.com` and `home.example.co.uk`.

### Edit a response before it reaches the client

```bash
rflow --edit-resp httpbin.org
```

```bash
curl -i -x http://localhost:8080 --cacert rflow-ca.pem https://httpbin.org/get
```

The editor opens with the response:

```
HTTP/1.1 200 OK
content-type: application/json
...

{ "args": {}, ... }
```

Change the status to `418` or edit the body. curl receives what you saved.

### Edit requests to one site and responses from another

The two edit flags are independent:

```bash
rflow --edit-req postman-echo.com --edit-resp httpbin.org
```

Requests to postman-echo.com open in the editor before they are sent. Responses from httpbin.org open in the editor before they reach the client. Everything else passes straight through.

### See what was actually sent and returned

Combine editing and printing. Printing always shows the message after your edits: the request as it was sent upstream, and the response as the client received it.

```bash
rflow --edit-req httpbin.org --print-req --print-resp
```

### Proxy a whole shell session

Most command-line tools respect the standard proxy variables:

```bash
export http_proxy=http://localhost:8080 https_proxy=http://localhost:8080
export SSL_CERT_FILE=$PWD/rflow-ca.pem   # or CURL_CA_BUNDLE for curl only
```

## Notes

- Browsers usually ask for compressed responses. Compressed bodies are shown as binary when printed, and as unreadable text when edited. curl only asks for compression when given `--compressed`.
- If you change the host in an edited request's URL, update the `host` header to match.
- Editing uses `$EDITOR`, which may include arguments such as `code --wait`. If it is unset, rflow uses `vim`. The editor must not return until the file is closed.
- Only one edit is open at a time. Other edits wait until it is finished.
