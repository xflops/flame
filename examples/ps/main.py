from ps import ConvNet, get_data_loader, ParameterServer, DataWorker, evaluate
from flamepy import Runner


if __name__ == "__main__":
    model = ConvNet()
    test_loader = get_data_loader()[1]
    print("Running synchronous parameter server training.")

    with Runner("ps-example") as rr:
        ps_svc = rr.service(ParameterServer(1e-2))
        workers_svc = [rr.service(DataWorker) for _ in range(2)]

        current_weights = ps_svc.get_weights().get()
        for i in range(20):
            gradients = [worker.compute_gradients(current_weights) for worker in workers_svc]
            # Calculate update after all gradients are available.
            current_weights = ps_svc.apply_gradients(*gradients).get()

            if i % 10 == 0:
                # Evaluate the current model.
                model.set_weights(current_weights)
                accuracy = evaluate(model, test_loader)
                print("Iter {}: \taccuracy is {:.1f}".format(i, accuracy))

        print("Final accuracy is {:.1f}.".format(accuracy))
