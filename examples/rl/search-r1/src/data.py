"""Questions, corpus, prompt format and reward for the Search-R1 loop."""

import re
import string
from typing import Optional

PROMPT = (
    "Answer the given question. You must conduct reasoning inside <think> and </think> "
    "first every time you get new information. After reasoning, if you find you lack some "
    "knowledge, you can call a search engine by <search> query </search> and it will return "
    "the top searched results between <information> and </information>. You can search as "
    "many times as your want. If you find no further external knowledge needed, you can "
    "directly provide the answer inside <answer> and </answer>, without detailed "
    "illustrations. For example, <answer> Beijing </answer>. Question: {question}\n"
)


def normalize_answer(text: str) -> str:
    text = text.lower()
    text = "".join(ch for ch in text if ch not in set(string.punctuation))
    text = re.sub(r"\b(a|an|the)\b", " ", text)
    return " ".join(text.split())


def exact_match(prediction: Optional[str], golds: list[str]) -> bool:
    if prediction is None:
        return False
    normalized = normalize_answer(prediction)
    return any(normalized == normalize_answer(gold) for gold in golds)


def last_answer(text: str) -> Optional[str]:
    """The text of the last <answer>...</answer> block, or None."""
    matches = re.findall(r"<answer>(.*?)</answer>", text, re.DOTALL)
    return matches[-1].strip() if matches else None


def load_hotpotqa(train: int, dev: int, seed: int = 0):
    """HotpotQA questions plus a retrieval corpus made of their context paragraphs.

    Returns (train_questions, dev_questions, corpus); a question is
    {"id", "question", "answers"} and a corpus entry is "Title: paragraph".
    """
    import random

    from datasets import load_dataset

    def rows(split, count):
        data = load_dataset("hotpotqa/hotpot_qa", "distractor", split=split)
        order = list(range(len(data)))
        random.Random(seed).shuffle(order)
        return [data[i] for i in order[:count]]

    train_rows, dev_rows = rows("train", train), rows("validation", dev)
    corpus, seen = [], set()
    for row in train_rows + dev_rows:
        for title, sentences in zip(row["context"]["title"], row["context"]["sentences"]):
            passage = f"{title}: {''.join(sentences).strip()}"
            if passage not in seen:
                seen.add(passage)
                corpus.append(passage)

    def question(row):
        return {"id": row["id"], "question": row["question"], "answers": [row["answer"]]}

    return [question(r) for r in train_rows], [question(r) for r in dev_rows], corpus
