# vLLM Examples

- [offline](offline/README.md): `flamepy.app` class service demonstrating retained
  model state and data-aware prefix-cache reuse.
- [serving](serving/README.md): `flamepy.serving` instance with a client that
  submits batched generation requests through Flame sessions.

Both examples use `facebook/opt-125m` and request one GPU. Run them from a checkout of this
repository, in `examples/vllm`.
