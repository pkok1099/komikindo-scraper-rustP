#!/usr/bin/env python3
"""Train title detection LM using sklearn → export ONNX.

Model: 3-layer MLP (30 → 64 → 32 → 1)
Loss: Binary Cross-Entropy (logistic)
Output: Probability that a DOM node is the title

Usage:
  python3 train_model.py --input training_data.npz --output title_detector.onnx
"""

import argparse
import json
import numpy as np
from pathlib import Path

from sklearn.neural_network import MLPClassifier
from sklearn.model_selection import cross_val_score, train_test_split
from sklearn.metrics import classification_report, confusion_matrix
from sklearn.preprocessing import StandardScaler
import onnx
from onnx import helper, TensorProto, numpy_helper


FEATURE_NAMES = [
    'is_h1', 'is_h2', 'is_h3', 'is_span', 'is_div', 'is_a', 'is_td',
    'has_class_title', 'has_class_entry', 'has_class_info',
    'has_id',
    'depth', 'sibling_index', 'sibling_count', 'child_count', 'text_length',
    'text_starts_komik',
    'parent_is_div', 'parent_has_info',
    'has_itemprop',
    'is_inside_spe', 'is_inside_infox', 'is_inside_infoanime',
    'bold_text_ratio', 'link_count', 'has_rel_tag',
    'text_word_count', 'has_img_child',
    'is_first_significant', 'font_size_indicator',
]

NUM_FEATURES = len(FEATURE_NAMES)


def sklearn_mlp_to_onnx(clf, scaler, input_size):
    """Convert sklearn MLPClassifier to ONNX model manually.

    Architecture: Input → StandardScaler → MLP → Sigmoid → Output
    """
    # Build ONNX graph nodes
    nodes = []
    initializers = []

    # Input
    input_tensor = helper.make_tensor_value_info('features', TensorProto.FLOAT, [None, input_size])
    output_tensor = helper.make_tensor_value_info('probability', TensorProto.FLOAT, [None, 1])

    # Step 1: StandardScaler (scale + offset)
    scale_np = scaler.scale_.astype(np.float32)
    mean_np = scaler.mean_.astype(np.float32)

    scale_init = numpy_helper.from_array(scale_np, name='scaler_scale')
    mean_init = numpy_helper.from_array(mean_np, name='scaler_mean')
    initializers.extend([scale_init, mean_init])

    # normalized = (features - mean) / scale
    sub_node = helper.make_node('Sub', ['features', 'scaler_mean'], ['centered'], name='scaler_sub')
    div_node = helper.make_node('Div', ['centered', 'scaler_scale'], ['normalized'], name='scaler_div')
    nodes.extend([sub_node, div_node])

    # Step 2: MLP layers
    # sklearn MLP stores: coefs_ (list of weight matrices) and intercepts_ (list of bias vectors)
    prev_output = 'normalized'

    for i, (weight, bias) in enumerate(zip(clf.coefs_, clf.intercepts_)):
        is_last = (i == len(clf.coefs_) - 1)

        weight_name = f'layer{i}_weight'
        bias_name = f'layer{i}_bias'
        matmul_output = f'layer{i}_matmul'
        add_output = f'layer{i}_add'

        # Weight: sklearn stores as (input, output), ONNX MatMul needs same
        weight_init = numpy_helper.from_array(weight.astype(np.float32), name=weight_name)
        bias_init = numpy_helper.from_array(bias.astype(np.float32), name=bias_name)
        initializers.extend([weight_init, bias_init])

        # MatMul: [batch, input] × [input, output] = [batch, output]
        matmul_node = helper.make_node('MatMul', [prev_output, weight_name], [matmul_output], name=f'matmul_{i}')

        # Add bias
        add_node = helper.make_node('Add', [matmul_output, bias_name], [add_output], name=f'add_{i}')
        nodes.extend([matmul_node, add_node])

        # Activation: ReLU for hidden layers only
        if not is_last:
            act_output = f'layer{i}_act'
            act_node = helper.make_node('Relu', [add_output], [act_output], name=f'relu_{i}')
            nodes.append(act_node)
            prev_output = act_output
        else:
            # Last layer: output goes directly to sigmoid
            prev_output = add_output

    # Step 3: Sigmoid (convert logits to probability)
    # prev_output now points to the last layer's add output
    sigmoid_node = helper.make_node('Sigmoid', [prev_output], ['probability'], name='sigmoid')
    nodes.append(sigmoid_node)

    # Create graph
    graph = helper.make_graph(nodes, 'title_detector', [input_tensor], [output_tensor], initializers)

    # Create model
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid('', 17)])
    model.ir_version = 8

    # Validate
    onnx.checker.check_model(model)

    return model


def main():
    parser = argparse.ArgumentParser(description='Train title detection model')
    parser.add_argument('--input', type=str, default='training_data.npz', help='Training data .npz file')
    parser.add_argument('--output', type=str, default='title_detector.onnx', help='Output ONNX file')
    parser.add_argument('--hidden-sizes', type=str, default='64,32', help='Hidden layer sizes')
    parser.add_argument('--epochs', type=int, default=300, help='Max iterations')
    parser.add_argument('--cv', type=int, default=0, help='Cross-validation folds (0=none)')
    args = parser.parse_args()

    # Load data
    data = np.load(args.input, allow_pickle=True)
    X = data['X']
    y = data['y']

    print(f"Loaded training data: {X.shape[0]} samples, {X.shape[1]} features")
    print(f"  Title nodes: {y.sum()} ({y.sum()/len(y)*100:.2f}%)")
    print(f"  Non-title nodes: {len(y) - y.sum()}")

    # Split train/test
    X_train, X_test, y_train, y_test = train_test_split(X, y, test_size=0.2, random_state=42, stratify=y)

    print(f"\nTrain: {len(X_train)} | Test: {len(X_test)}")

    # StandardScaler
    scaler = StandardScaler()
    X_train_scaled = scaler.fit_transform(X_train)
    X_test_scaled = scaler.transform(X_test)

    # Parse hidden sizes
    hidden_sizes = tuple(int(s) for s in args.hidden_sizes.split(','))
    print(f"\nModel: MLP ({X.shape[1]} → {' → '.join(str(s) for s in hidden_sizes)} → 1)")
    print(f"Max iterations: {args.epochs}")

    # Train
    clf = MLPClassifier(
        hidden_layer_sizes=hidden_sizes,
        activation='relu',
        solver='adam',
        max_iter=args.epochs,
        random_state=42,
        early_stopping=True,
        validation_fraction=0.1,
        n_iter_no_change=20,
        verbose=True,
    )
    clf.fit(X_train_scaled, y_train)

    # Evaluate
    y_pred = clf.predict(X_test_scaled)
    y_prob = clf.predict_proba(X_test_scaled)[:, 1]

    print("\n" + "=" * 60)
    print("EVALUATION ON TEST SET")
    print("=" * 60)
    print(classification_report(y_test, y_pred, target_names=['non-title', 'title']))

    cm = confusion_matrix(y_test, y_pred)
    print(f"Confusion Matrix:")
    print(f"  TN={cm[0][0]:5d}  FP={cm[0][1]:5d}")
    print(f"  FN={cm[1][0]:5d}  TP={cm[1][1]:5d}")

    # Cross-validation
    if args.cv > 0:
        print(f"\n{args.cv}-fold Cross-Validation...")
        X_all_scaled = scaler.fit_transform(X)
        scores = cross_val_score(clf, X_all_scaled, y, cv=args.cv, scoring='f1')
        print(f"  F1 scores: {scores}")
        print(f"  Mean F1: {scores.mean():.4f} ± {scores.std():.4f}")

    # Feature importance (via weight magnitude in first layer)
    first_layer_weights = np.abs(clf.coefs_[0]).mean(axis=1)
    importance = sorted(zip(FEATURE_NAMES, first_layer_weights), key=lambda x: -x[1])
    print("\nFeature Importance (first layer weight magnitude):")
    for name, imp in importance[:10]:
        bar = '█' * int(imp * 50)
        print(f"  {name:25s} {imp:.4f} {bar}")

    # Export ONNX
    print(f"\nExporting to ONNX: {args.output}")
    # Refit scaler on ALL data
    scaler_full = StandardScaler()
    scaler_full.fit(X)

    onnx_model = sklearn_mlp_to_onnx(clf, scaler_full, X.shape[1])

    output_path = Path(args.output)
    onnx.save(onnx_model, output_path)

    # Verify ONNX model
    import onnxruntime as ort
    sess = ort.InferenceSession(str(output_path))
    input_name = sess.get_inputs()[0].name
    output_name = sess.get_outputs()[0].name

    # Test with a sample
    sample = X_test_scaled[:5].astype(np.float32)
    onnx_pred = sess.run([output_name], {input_name: sample})[0]
    sklearn_pred = clf.predict_proba(sample)[:, 1:2]

    print(f"\nONNX verification (5 samples):")
    print(f"  sklearn: {sklearn_pred.flatten()}")
    print(f"  onnx:    {onnx_pred.flatten()}")
    print(f"  max diff: {np.abs(sklearn_pred.flatten() - onnx_pred.flatten()).max():.6f}")

    model_size = output_path.stat().st_size
    print(f"\nModel size: {model_size:,} bytes ({model_size/1024:.1f} KB)")
    print(f"Saved to: {output_path}")

    # Also save metadata
    meta = {
        'feature_names': FEATURE_NAMES,
        'num_features': NUM_FEATURES,
        'hidden_sizes': list(hidden_sizes),
        'scaler_mean': scaler_full.mean_.tolist(),
        'scaler_scale': scaler_full.scale_.tolist(),
        'training_samples': int(len(X)),
        'title_samples': int(y.sum()),
        'test_accuracy': float((y_pred == y_test).mean()),
    }
    meta_path = output_path.with_suffix('.json')
    with open(meta_path, 'w') as f:
        json.dump(meta, f, indent=2)
    print(f"Metadata saved to: {meta_path}")


if __name__ == '__main__':
    main()
