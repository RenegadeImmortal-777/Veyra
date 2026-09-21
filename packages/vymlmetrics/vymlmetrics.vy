fn accuracy(actual: List, predicted: List) -> Number { return accuracy_score(actual, predicted); }
fn classification(actual: List, predicted: List) -> Map { return classification_metrics(actual, predicted); }
fn train_test(x: List, y: List, test_ratio: Number) -> Map { return train_test_split(x, y, test_ratio); }
