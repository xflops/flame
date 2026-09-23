from ps import ConvNet, DataWorker, ParameterServer, evaluate, get_data_loader

import flamepy.app as app

app.init("ps-example")


@app.service(autoscale=False, warmup=1)
class ParameterServerService(ParameterServer):
    def __init__(self):
        super().__init__(1e-2)


@app.service(warmup=2)
class DataWorkerService(DataWorker):
    pass


if __name__ == "__main__":
    model = ConvNet()
    test_loader = get_data_loader()[1]
    print("Running synchronous parameter server training.")

    try:
        ps_svc = ParameterServerService()
        worker_svc = DataWorkerService()
        workers_svc = [worker_svc, worker_svc]

        current_weights = ps_svc.get_weights().get()
        for i in range(20):
            gradients = [
                worker.compute_gradients(current_weights) for worker in workers_svc
            ]
            # Calculate update after all gradients are available.
            current_weights = ps_svc.apply_gradients(*gradients).get()

            if i % 10 == 0:
                # Evaluate the current model.
                model.set_weights(current_weights)
                accuracy = evaluate(model, test_loader)
                print("Iter {}: \taccuracy is {:.1f}".format(i, accuracy))

        print("Final accuracy is {:.1f}.".format(accuracy))
    finally:
        app.destroy()
