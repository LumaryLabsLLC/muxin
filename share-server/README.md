# Muxin share server

Introduces the two sides of a shared session (WebRTC signaling) and serves
the page that joins one from a browser. muxin.dev does the same for
everyone; this is for running your own, and for tests. It never sees
what's shared: the share's secret is after the `#` in the link, which
browsers don't send, and both sides check each other's certificates end to
end. The interface (a few JSON calls) is described in `src/share/signal.rs`.

```sh
cargo run -p muxin-share-server            # http://127.0.0.1:8787
MUXIN_SHARE_SERVER=http://127.0.0.1:8787 cargo run    # Muxin, sharing through it
```

## Running it for real

Browsers need https, so put it behind a proxy with TLS. With Caddy:

```
share.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Most connections are direct. For the rest (strict NATs, some corporate
networks), run a TURN relay such as coturn and tell the server about it:

```
# /etc/turnserver.conf
use-auth-secret
static-auth-secret=<a long random secret>
realm=share.example.com
listening-port=3478
fingerprint
no-cli
```

```sh
MUXIN_SHARE_LISTEN=127.0.0.1:8787 \
MUXIN_SHARE_STUN=stun:share.example.com:3478 \
MUXIN_SHARE_TURN=turn:share.example.com:3478 \
MUXIN_SHARE_TURN_SECRET=<the same secret> \
muxin-share-server
```

Each side gets TURN credentials of its own that last a day (coturn's REST
scheme). The relay only ever carries encrypted traffic.

## Endpoints

- `/s`: the join page (the link is `https://<server>/s#1.<ticket>`), with its files under `/share/`.
- `/share/v1/...`: the introductions (see `src/share/signal.rs`).
- `/healthz`: `ok`.

## muxin.dev

The website serves the same page from its own repository (`public/share/`
and `resources/views/share/join.blade.php`) and the same interface
(`app/Share`). Copy changes to `web/` there too.
