fn linear_fit(x: List, y: List) -> Map { return linreg_fit(x, y); }
fn linear_predict(model: Map, x: Number) -> Number { return linreg_predict(model, x); }
fn logistic_fit(features: List, labels: List, iterations: Number, learning_rate: Number) -> Map {
    return logreg_fit(features, labels, iterations, learning_rate);
}
fn logistic_predict(model: Map, features: List) -> Number { return logreg_predict(model, features); }
fn nearest_class(train_x: List, train_y: List, point: List, k: Int) -> Any {
    return knn_predict(train_x, train_y, point, k);
}
fn clusters(data: List, k: Int, iterations: Int) -> List { return kmeans(data, k, iterations); }
fn normalize_values(xs: List) -> List { return normalize(xs); }
fn dense_layer(input: List, weights: List, bias: List) -> List { return dense(input, weights, bias); }
fn relu_layer(xs: List) -> List { return relu(xs); }
fn probabilities(xs: List) -> List { return softmax(xs); }
fn logistic(x: Number) -> Number { return sigmoid(x); }
fn mean_squared_error(actual: List, predicted: List) -> Number { return mse(actual, predicted); }
