import io
import re
from urllib.parse import urljoin, urlparse

import markitdown
import requests
from bs4 import BeautifulSoup
from flamepy import service

from apis import Summary, WebPage

ins = service.FlameInstance()

headers = {"User-Agent": "Xflops Crawler 1.0", "From": "support@xflops.io"}


def get_encoding(soup):
    encoding = None
    if soup:
        for meta_tag in soup.find_all("meta"):
            encoding = meta_tag.get("charset")
            if encoding:
                break

            encoding = meta_tag.get("content-type")
            if encoding:
                break

            content = meta_tag.get("content")
            if content:
                match = re.search("charset=(.*)", content)
                if match:
                    encoding = match.group(1)
                    break
    if encoding:
        return str(encoding).lower()

    return "utf-8"


@ins.entrypoint
def crawler_app(wp: WebPage) -> Summary:
    text = requests.get(wp.url, headers=headers).text

    soup = BeautifulSoup(text, "html.parser")
    encoding = get_encoding(soup)

    links = []
    for link in soup.find_all("a"):
        href = link.get("href")
        if href is None:
            continue
        u = urlparse(href)
        if u.scheme == "http" or u.scheme == "https":
            links.append(u.geturl())
        elif u.scheme == "":
            links.append(urljoin(wp.url, href))

    links = list(set(links))

    md = markitdown.MarkItDown()
    stream = io.BytesIO(text.encode(encoding))
    result = md.convert(stream).text_content

    return Summary(links=links, content=result)


if __name__ == "__main__":
    ins.run()
