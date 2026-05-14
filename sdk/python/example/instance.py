# /// script
# dependencies = [
#   "flamepy",
# ]
# [tool.uv.sources]
# flamepy = { path = ".." }
# ///

"""
Example usage of the Flame Python SDK instance functionality.
"""

from dataclasses import dataclass

from flamepy import service


@dataclass
class SysPrompt:
    prompt: str


@dataclass
class Blog:
    url: str


@dataclass
class Summary:
    url: str
    summary: str


ins = service.FlameInstance()


@ins.entrypoint
def summarize_blog(bl: Blog) -> Summary:
    sys_prompt = """
    You are a helpful assistant.
    """

    ctx = ins.context()
    if ctx is not None and isinstance(ctx, SysPrompt):
        sys_prompt = ctx.prompt

    summary = f"Summary of {bl.url}: {sys_prompt}"

    return Summary(url=bl.url, summary=summary)


ins.run()
