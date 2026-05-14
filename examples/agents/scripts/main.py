from flamepy.service import Session
from openai import OpenAI
import os

tools = [
    {
        "type": "function",
        "function": {
            "name": "run_script",
            "description": "Run a script, the user shoud supply a script name and parameters",
            "parameters": {
                "type": "object",
                "properties": {
                    "language": {
                        "type": "string",
                        "description": "The language of the script, e.g. python",
                    },
                    "code": {
                        "type": "string",
                        "description": "The code of the script to run, e.g. print('Hello, world!')",
                    },
                },
                "required": ["language", "code"],
            },
        },
    },
]

client = OpenAI(
    api_key=os.getenv("DEEPSEEK_API_KEY"), base_url="https://api.deepseek.com"
)


def send_messages(messages):
    response = client.chat.completions.create(
        model="deepseek-chat", messages=messages, tools=tools
    )
    return response.choices[0].message


def main():
    session = Session("flmexec")

    # 1. Ask DeepSeek to generate a script
    messages = [
        {
            "role": "user",
            "content": "Provided a Python snippet that computes and prints the sum of integers from 1 to 100.",
        }
    ]
    message = send_messages(messages)
    messages.append(message)
    print(f"Model>\t {message.content}")

    # 2. Ask DeepSeek to call the tool to run the script
    message = {"role": "user", "content": "run this code"}
    messages.append(message)
    message = send_messages(messages)

    tool = message.tool_calls[0]
    messages.append(message)

    # 3. Call the tool to run the script and get the result for DeepSeek to see
    input = tool.function.arguments.encode("utf-8")
    result = session.invoke(input)

    # 4. Ask DeepSeek to summarize the result
    messages.append(
        {"role": "tool", "tool_call_id": tool.id, "content": result.decode("utf-8")}
    )
    message = send_messages(messages)
    print(f"Model>\t {message.content}")

    session.close()


if __name__ == "__main__":
    main()
