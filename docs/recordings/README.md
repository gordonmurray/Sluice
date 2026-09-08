# Payment recording

`payment-demo.cast` is an asciinema recording of live requests to the offline demo.
The README embeds its GIF render directly. GitHub does not run asciinema's JavaScript
player in README files; the animated image needs no external player.

The recording uses mock USDC and public local test credentials. It does not contain
private keys or payment signatures. The presentation script decodes headers and
shows the first search result to keep the terminal readable. All request outcomes,
transaction checks, balance changes, and indexed receipt values come from the running stack.
Reading pauses are included; this recording is not a latency benchmark.

Hosted playback is available on [asciinema](https://asciinema.org/a/g0e9FsGFO4vL6vNA).
The README animation and local replay do not depend on that hosted copy.

## Replay

```sh
asciinema play docs/recordings/payment-demo.cast
```

## Record again

Requirements: Docker Compose, Bash, curl, Python 3, and asciinema 2.x.
Run these commands from the repository root. Keep the default demo pricing and
caller configuration, and do not send other paid traffic during the recording.

```sh
docker compose -f docker-compose.yml -f docker-compose.offline.yml up -d --build
```

Wait for the gateway, facilitator, and indexer to become ready. Then record:

```sh
TERM=xterm-256color asciinema rec --overwrite \
  --cols 100 --rows 30 --env TERM \
  --title 'Sluice: a paid HTTP request, end to end' \
  --command 'python3 scripts/record-demo.py' \
  docs/recordings/payment-demo.cast
```

The script checks the advertised mock-token address before signing. It makes one
paid request and checks the transaction, receiver balance change, and indexed receipt.
A failed check stops the presentation. Inspect the recording before publishing it.

Render the GIF with the same pinned agg image used for the checked-in version:

```sh
docker run --rm -v "$PWD:/data" \
  ghcr.io/asciinema/agg@sha256:84e04c21013e4fb91cbdb3eade5977c1e66685c18f0d234da78d3350bb3404b2 \
  --theme github-dark --font-size 16 --idle-time-limit 20 --last-frame-duration 8 \
  /data/docs/recordings/payment-demo.cast /data/docs/images/payment-demo.gif
```

Stop the stack after recording:

```sh
docker compose -f docker-compose.yml -f docker-compose.offline.yml down
```
