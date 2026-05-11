# Estimating the value of Pi using Monte Carlo

## Motivation

Monte Carlo methods, or Monte Carlo Simulation are a broad class of computational algorithms that rely on repeated random sampling to obtain numerical results. The underlying concept is to use randomness to solve problems that might be deterministic in principle. Monte Carlo methods are used in many user scenarios, such as finance, online gaming, science, mathematics, engineering and so on; this blog demonstrates Monte Carlo methods by Pi with Flame.

<img alt="Pi_30k" style="float: right" src="../images/Pi_30K.gif"/>

Considering a quadrant (circular sector) inscribed in a unit square; given that the ratio of their areas is `π/4`, the value of `π` can be approximated using a Monte Carlo method:

  * Draw a square, then inscribe a quadrant within it
  * Uniformly scatter a given number of points over the square
  * Count the number of points inside the quadrant, i.e. having a distance from the origin of less than 1

The ratio of the inside-count and the total-sample-count is an estimate of the ratio of the two areas, `π/4`. Multiply the result by 4 to estimate `π`.

In this procedure the domain of inputs is the square that circumscribes the quadrant. We generate random inputs by scattering grains over the square then perform a computation on each input (test whether it falls within the quadrant). Aggregating the results yields our final result, the approximation of `π`.

There are two important considerations:

  1. If the points are not uniformly distributed, then the approximation will be poor.
  2. The approximation is generally poor if only a few points are randomly placed in the whole square. On average, the approximation improves as more points are placed.

Uses of Monte Carlo methods require large amounts of random numbers, and their use benefitted greatly from pseudorandom number generators, which were far quicker to use than the tables of random numbers that had been previously used for statistical sampling.

## Estimating Pi locally

According to the description above, it's straight forward to approximate the value of `π` by following steps:

1. Generate two number randomly as coordinates of the point.
1. Calculate the distance of the point by  $\sqrt{x^2+y^2}$; if the distance is less than the radius of circle, counting it as circle's area.
1. Continue step-1 and step-2 according to programe's arguments, e.g. $10^7$.
1. Approximate the value of `π` by $4*circle/square$, the square is the input of programe's argumment.

Thanks to Rust `rand` model, it's easy to generte coordinates randomly; so the value of `π` is approximated as follow, the full source code can be downloaded from [Flame repo](../../examples/pi/src/local/).

```rust
    let mut area = 0.0;
    for _ in 0..total as i64 {
        let x: f64 = die.sample(&mut rng);
        let y: f64 = die.sample(&mut rng);
        let dist = (x * x + y * y).sqrt();

        if dist <= 1.0 {
            area += 1.0;
        }
    }

    let pi = 4_f64 * area / total;
```

After compiled it with `cargo build`, let's run it multiple times to approximate the value of `π`. As shown in the output, the more point number, the more accurate the approximation; accordingly, the more time.

```shell
$ time ./target/debug/pi-local.exe --point-num 10000000 # 10^7
pi = 4*(7852140/10000000) = 3.140856

real    0m19.532s
user    0m0.000s
sys     0m0.000s

$ time ./target/debug/pi-local.exe --point-num 100000000 # 10^8
pi = 4*(78538612/100000000) = 3.14154448

real    1m21.517s
user    0m0.000s
sys     0m0.000s

$ time ./target/debug/pi-local.exe --point-num 1000000000 # 10^9
pi = 4*(785411939/1000000000) = 3.141647756

real    13m43.092s
user    0m0.078s
sys     0m0.031s

```

## Scale up by Flame 

### Why Flame?

Flame is a distributed system for intelligent workloads, it supports any application with **low latency**. It provides different shim, e.g. stdio shim, to integrate with applications, which make it easy to migrate the local `π` approximation programe to Flame, and get the benifit of low latency distributed system.

### Pi in Flame

To migrate local `π` into Flame, a new client is introduced to submit Monte Carlo tasks and aggregate task outputs from the server, and approximate the value of `π` in client. In the server side, it only calculates how many points are in the circle as output.

#### Pi Client

To submit tasks to Flame, the client need to connect to the Flame cluster by the APIs. If successful, a `flame::Connection` object is created; it's used to talk with Flame.

```rust
let conn = flame::connect("http://127.0.0.1:8080").await?;
```

After connected to Flame cluster, `flame::Connection` is used to create `Session` for tasks as follow; `flame::SessionAttributes` provides the information on how to create a session — e.g. which application to run, and the per-task `resreq` (cpu / memory / gpu).

```rust
    let attr = SessionAttributes {
        application: app,
        resreq: Some(ResourceRequirement { cpu: 1, memory: 1 * 1024 * 1024 * 1024, gpu: 0 }),
        ..Default::default()
    };

    let ssn = conn.create_session(&attr).await?;
```

There're two ways to submit tasks and retrieve tasks outputs:

1. Submit task input to create task by `flame::Session::create_task`, and then keep calling `flame::Session::get_task()` to check task status; when it's succeed, retrieve task output accordingly.
1. Submit task input with an informer `flame::TaskInformer` to create task by `flame::Session::run_task`; when the task status chagned, a callback function in `flame::TaskInformer` will be triggerred.

In this example, the second way is used. In the callback function, if there's an output (only generated when task completed), consider it into circle's area.

```rust
pub struct PiInfo {
    pub area: i64,
}

impl TaskInformer for PiInfo {
    fn on_update(&mut self, task: Task) {
        if let Some(output) = task.output {
            let output_str = String::from_utf8(output.to_vec()).unwrap();
            self.area += output_str.trim().parse::<i64>().unwrap();
        }
    }

    fn on_error(&mut self, _: FlameError) {
        print!("Got an error")
    }
}
```

The `flame::Session::run_task` will return a `Future`, so `try_join_all().await` is leveraged to wait for all tasks completed.

```rust
    let mut tasks = vec![];
    for _ in 0..task_num {
        let task_input = task_input_str.as_bytes().to_vec();
        let task = ssn.run_task(Some(TaskInput::from(task_input)), informer.clone());
        tasks.push(task);
    }

    try_join_all(tasks).await?;
```

After all tasks completed, the value of `π` is approximated by the area of circle and the area of square: the area of circle is the summary of task's output, the area of square is the input of all tasks.

```rust
    {
        // Get the number of points in the circle.
        let informer = flame::lock_ptr!(informer)?;
        let pi = 4_f64 * informer.area as f64 / ((task_num as f64) * (task_input as f64));

        println!(
            "pi = 4*({}/{}) = {}",
            informer.area,
            task_num * task_input,
            pi
        );
    }
```

#### Pi Server

The Pi application server is similar with the local version; the only different is moving the `π` calculation to the client side, the server only return the number of points in the circle.

```rust
    let total = input.trim().parse::<i32>()?;
    let mut sum = 0;

    for _ in 0..total {
        let x: f64 = die.sample(&mut rng);
        let y: f64 = die.sample(&mut rng);
        let dist = (x * x + y * y).sqrt();

        if dist <= 1.0 {
            sum += 1;
        }
    }

    println!("{}", sum);

```

#### Output

After build the client & server of Pi application, deploy and run the Pi application in a Flame cluster with 6 executors as follow. As shown in the output of Pi client, it only takes about 2 minutes to approximate `π` with $10^9$.

```shell
[klausm@hpc-cloud02 flame]$ time ./target/debug/pi --task-num 10000 --task-input 100000 # 10^9
pi = 4*(785388765/1000000000) = 3.14155506

real    1m51.708s
user    0m10.626s
sys     0m1.286s
```

```shell
[klausm@hpc-cloud02 flame]$ ./target/debug/flmctl list
ID        State     App            Resources                  Priority  Pending   Running   Succeed   Failed    Created
7         Open      pi             cpu=1,mem=1g,gpu=0         0         8955      6         1039      0         02:01:48
1         Closed    pi             cpu=1,mem=1g,gpu=0         0         0         0         100       0         01:55:58
2         Closed    pi             cpu=1,mem=1g,gpu=0         0         0         0         10000     0         01:56:24
3         Closed    pi             cpu=1,mem=1g,gpu=0         0         0         0         10000     0         01:56:48
4         Closed    pi             cpu=1,mem=1g,gpu=0         0         0         0         1000      0         01:57:13
5         Closed    pi             cpu=1,mem=1g,gpu=0         0         0         0         100       0         01:57:38
6         Closed    pi             cpu=1,mem=1g,gpu=0         0         0         0         10000     0         01:58:22
```

## Reference

* Pi Example: https://github.com/flame-sh/flame/tree/main/examples/pi
* Github: https://github.com/flame-sh/flame
* Dockerhub: https://hub.docker.com/u/xflops
