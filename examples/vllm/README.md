# vLLM Examples

- [offline](offline/README.md): `flamepy.app` class service demonstrating retained
  model state and data-aware prefix-cache reuse.
- [serving](serving/README.md): `flamepy.serving` instance with a client that
  submits batched generation requests through Flame sessions.

Both examples use `facebook/opt-125m`, request one GPU, and are installed by
`flmadm install --client --with-examples` under `$FLAME_HOME/examples/vllm`.
