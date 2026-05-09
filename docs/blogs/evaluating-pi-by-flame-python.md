# Estimating the value of Pi by Flame Python Client

## Motivation

The last blog, ["Estimating the value of Pi using Monte Carlo"](evaluating-pi-by-monte-carlo.md), 
demonstrates the way on evaluating the value of Pi by Rust client.
As the Python is another common programing language for data analysis, Flame also provide Python client
to launch tasks in Flame cluster. This blog demonstrates how to use Flame Python client to launch tasks.

## Flame Python Client Example

Thanks to gRPC, Flame support launch tasks in different language, comparing to the service. So this blog reuse the
Rust service of Pi in ["Estimating the value of Pi using Monte Carlo"](evaluating-pi-by-monte-carlo.md), and build a
new Python client to launch tasks.

### Read the number of tasks

The `argparse` is used to parse the parameters of Python client. There are two parameters: 
'the number of tasks' and 'the input of each task'. And then, build a list of task inputs.

```python
parser = argparse.ArgumentParser(description='Flame Pi Python Example.')
parser.add_argument('-n', '--task_num', type=int, help="The total number of tasks in the session.")
parser.add_argument('-i', '--task_input', type=int, help="The input of each task to calcuate Pi.")
args = parser.parse_args()

# Convert args.task_input into bytes type.
task_input = str(args.task_input).encode()
task_inputs = [task_input] * args.task_num
```

### Connect to Flame cluster

Similar to Rust client, it's time to connect to the Flame cluster and create a session for coming tasks.

```python
conn = flame.connect("127.0.0.1:8080")
ssn = conn.create_session(application="pi", resreq={"cpu": 1, "mem": "1g"})
```

### Run sample tasks

In python client, `run_all_tasks` is introduced to simplify the tasks submission. In this example, it accept two parameters:
a list of task input and a callback on each task completed. In addition, there is an another callback, named `on_error`,
to handle errors during submission and running.

```python
area = 0.0


def get_circle_area(task):
    global area
    area += float(task.output)


# Submit all task inputs to Flame, and wait for the result.
ssn.run_all_tasks(task_inputs=task_inputs, on_completed=get_circle_area)
```

### Calculate the value of Pi

After `run_all_tasks`, the `area` includes the total number of points in circle; so the value of Pi is evaluated as follows: 

```python
# Calculate the Pi.
pi = 4 * area / (args.task_input * args.task_num)

print("pi = 4*({}/{}) = {}".format(area, args.task_input * args.task_num, pi))
```

### Close the session

After the calculation, it's ok to close the session to avoid resource leakage; but before the close, we can re-launch the tasks
as many as need.

```python
ssn.close()
```

### Output

For now, all necessary codes are there; it's time to run it to evaluate the value of Pi.

```shell
[klausm@hpc-cloud02 flame]$ time python3 examples/pypi/main.py -i 100000 -n 1000
pi = 4*(78538813.0/100000000) = 3.14155252

real    0m11.485s
user    0m0.367s
sys     0m0.094s
```

## Reference

* [Estimating the value of Pi using Monte Carlo](evaluating-pi-by-monte-carlo.md)
* http://github.com/flame-sh/flame
* http://github.com/flame-sh/flame/example