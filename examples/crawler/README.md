# Crawler Example

This example registers a Flame application that downloads web pages, extracts a short text summary, and returns typed results to a Python client.

## Run

From the Flame console:

```bash
cd /opt/examples/crawler
flmctl register -f crawler-app.yaml
uv run client.py
```

## Files

- `crawler.py`: Flame service entrypoint.
- `client.py`: Submits a batch of URLs to the crawler application.
- `apis.py`: Shared request and response dataclasses.
- `crawler-app.yaml`: Application manifest.
