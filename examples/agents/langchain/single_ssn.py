from flamepy.serving import open_session
from apis import SysPrompt, Question

LANGCHAIN_AGENT_NAME = "langchain-agent"


def ask_agent():
    sys_prompt = SysPrompt(prompt="You are a weather forecaster.")
    question = Question(question="Who are you?")

    session = open_session(LANGCHAIN_AGENT_NAME, ctx=sys_prompt)
    output = session.run(question)

    print(output.answer)
    session.close()


if __name__ == "__main__":
    ask_agent()
