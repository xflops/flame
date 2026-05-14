from flamepy.service import Session
from apis import SysPrompt, Question

LANGCHAIN_AGENT_NAME = "langchain-agent"


def ask_agent():
    sys_prompt = SysPrompt(prompt="You are a weather forecaster.")
    question = Question(question="Who are you?")

    session = Session(LANGCHAIN_AGENT_NAME, ctx=sys_prompt)
    output = session.invoke(question)

    print(output.answer)
    session.close()


if __name__ == "__main__":
    ask_agent()
