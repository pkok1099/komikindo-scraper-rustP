#!/usr/bin/env python3
"""Train field detection LM using sklearn → export ONNX.

Supports multiple fields: title, rating

Model: 3-layer MLP (32 → 64 → 32 → 1)
Loss: Binary Cross-Entropy (logistic)
Output: Probability that a DOM node contains the target field

Usage:
  python3 train_model.py --field title
  python3 train_model.py --field rating
  python3 train_model.py --field all
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
    # Tag one-hot (0-8)
    'is_h1', 'is_h2', 'is_h3', 'is_span', 'is_div', 'is_a', 'is_td', 'is_i', 'is_meta',
    # Class features (9-13)
    'has_class_title', 'has_class_entry', 'has_class_info', 'has_class_rating', 'has_class_archive',
    # ID (14)
    'has_id',
    # Structural (15-19)
    'depth', 'sibling_index', 'sibling_count', 'child_count', 'text_length',
    # Text features (20)
    'text_starts_komik',
    # Parent features (21-22)
    'parent_is_div', 'parent_has_info',
    # Attribute features (23-24)
    'has_itemprop', 'itemprop_is_ratingValue',
    # Ancestor features (25-27)
    'is_inside_spe', 'is_inside_infox', 'is_inside_infoanime',
    # Content features (28-30)
    'bold_text_ratio', 'link_count', 'has_rel_tag',
    # Semantic features (31)
    'font_size_indicator',
]

NUM_FEATURES = len(FEATURE_NAMES)  # 32


def sklearn_mlp_to_onnx(clf, scaler, input_size):
    """Convert sklearn MLPClassifier to ONNX model manually.

    Architecture: Input → StandardScaler → MLP → Sigmoid → Output
    """
    nodes = []
    initializers = []

    input_tensor = helper.make_tensor_value_info('features', TensorProto.FLOAT, [None, input_size])
    output_tensor = helper.make_tensor_value_info('probability', TensorProto.FLOAT, [None, 1])

    # Step 1: StandardScaler (scale + offset)
    scale_np = scaler.scale_.astype(np.float32)
    mean_np = scaler.mean_.astype(np.float32)

    scale_init = numpy_helper.from_array(scale_np, name='scaler_scale')
    mean_init = numpy_helper.from_array(mean_np, name='scaler_mean')
    initializers.extend([scale_init, mean_init])

    sub_node = helper.make_node('Sub', ['features', 'scaler_mean'], ['centered'], name='scaler_sub')
    div_node = helper.make_node('Div', ['centered', 'scaler_scale'], ['normalized'], name='scaler_div')
    nodes.extend([sub_node, div_node])

    # Step 2: MLP layers
    prev_output = 'normalized'

    for i, (weight, bias) in enumerate(zip(clf.coefs_, clf.intercepts_)):
        is_last = (i == len(clf.coefs_) - 1)

        weight_name = f'layer{i}_weight'
        bias_name = f'layer{i}_bias'
        matmul_output = f'layer{i}_matmul'
        add_output = f'layer{i}_add'

        weight_init = numpy_helper.from_array(weight.astype(np.float32), name=weight_name)
        bias_init = numpy_helper.from_array(bias.astype(np.float32), name=bias_name)
        initializers.extend([weight_init, bias_init])

        matmul_node = helper.make_node('MatMul', [prev_output, weight_name], [matmul_output], name=f'matmul_{i}')
        add_node = helper.make_node('Add', [matmul_output, bias_name], [add_output], name=f'add_{i}')
        nodes.extend([matmul_node, add_node])

        if not is_last:
            act_output = f'layer{i}_act'
            act_node = helper.make_node('Relu', [add_output], [act_output], name=f'relu_{i}')
            nodes.append(act_node)
            prev_output = act_output
        else:
            prev_output = add_output

    # Step 3: Sigmoid
    sigmoid_node = helper.make_node('Sigmoid', [prev_output], ['probability'], name='sigmoid')
    nodes.append(sigmoid_node)

    # Create graph
    graph = helper.make_graph(nodes, 'field_detector', [input_tensor], [output_tensor], initializers)

    # Create model
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid('', 17)])
    model.ir_version = 8

    # Validate
    onnx.checker.check_model(model)

    return model


def train_field(field_name, input_path, output_dir, hidden_sizes, epochs, cv):
    """Train model for a specific field."""
    print(f"\n{'='*60}")
    print(f"  Training: {field_name}")
    print(f"{'='*60}")

    # Load data
    data = np.load(input_path, allow_pickle=True)
    X = data['X']
    y = data['y']

    print(f"Loaded training data: {X.shape[0]} samples, {X.shape[1]} features")
    print(f"  Positive ({field_name}) nodes: {y.sum()} ({y.sum()/len(y)*100:.2f}%)")
    print(f"  Negative nodes: {len(y) - y.sum()}")

    # Split train/test
    X_train, X_test, y_train, y_test = train_test_split(X, y, test_size=0.2, random_state=42, stratify=y)

    print(f"\nTrain: {len(X_train)} | Test: {len(X_test)}")

    # StandardScaler
    scaler = StandardScaler()
    X_train_scaled = scaler.fit_transform(X_train)
    X_test_scaled = scaler.transform(X_test)

    print(f"\nModel: MLP ({X.shape[1]} → {' → '.join(str(s) for s in hidden_sizes)} → 1)")
    print(f"Max iterations: {epochs}")

    # Train
    clf = MLPClassifier(
        hidden_layer_sizes=hidden_sizes,
        activation='relu',
        solver='adam',
        max_iter=epochs,
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
    print(f"EVALUATION ON TEST SET ({field_name})")
    print("=" * 60)
    print(classification_report(y_test, y_pred, target_names=[f'non-{field_name}', field_name]))

    cm = confusion_matrix(y_test, y_pred)
    print(f"Confusion Matrix:")
    print(f"  TN={cm[0][0]:5d}  FP={cm[0][1]:5d}")
    print(f"  FN={cm[1][0]:5d}  TP={cm[1][1]:5d}")

    # Cross-validation
    if cv > 0:
        print(f"\n{cv}-fold Cross-Validation...")
        X_all_scaled = scaler.fit_transform(X)
        scores = cross_val_score(clf, X_all_scaled, y, cv=cv, scoring='f1')
        print(f"  F1 scores: {scores}")
        print(f"  Mean F1: {scores.mean():.4f} ± {scores.std():.4f}")

    # Feature importance
    first_layer_weights = np.abs(clf.coefs_[0]).mean(axis=1)
    importance = sorted(zip(FEATURE_NAMES, first_layer_weights), key=lambda x: -x[1])
    print(f"\nFeature Importance (top 10 for {field_name}):")
    for name, imp in importance[:10]:
        bar = '█' * int(imp * 50)
        print(f"  {name:30s} {imp:.4f} {bar}")

    # Export ONNX
    output_path = Path(output_dir) / f'{field_name}_detector.onnx'
    print(f"\nExporting to ONNX: {output_path}")

    # Refit scaler on ALL data
    scaler_full = StandardScaler()
    scaler_full.fit(X)

    onnx_model = sklearn_mlp_to_onnx(clf, scaler_full, X.shape[1])

    onnx.save(onnx_model, output_path)

    # Verify ONNX model
    import onnxruntime as ort
    sess = ort.InferenceSession(str(output_path))
    input_name = sess.get_inputs()[0].name
    output_name = sess.get_outputs()[0].name

    sample = X_test_scaled[:5].astype(np.float32)
    onnx_pred = sess.run([output_name], {input_name: sample})[0]
    sklearn_pred = clf.predict_proba(sample)[:, 1:2]

    print(f"\nONNX verification (5 samples):")
    print(f"  sklearn: {sklearn_pred.flatten()}")
    print(f"  onnx:    {onnx_pred.flatten()}")
    print(f"  max diff: {np.abs(sklearn_pred.flatten() - onnx_pred.flatten()).max():.6f}")

    model_size = output_path.stat().st_size
    print(f"\nModel size: {model_size:,} bytes ({model_size/1024:.1f} KB)")

    # Save metadata
    meta = {
        'field': field_name,
        'feature_names': FEATURE_NAMES,
        'num_features': NUM_FEATURES,
        'hidden_sizes': list(hidden_sizes),
        'scaler_mean': scaler_full.mean_.tolist(),
        'scaler_scale': scaler_full.scale_.tolist(),
        'training_samples': int(len(X)),
        'positive_samples': int(y.sum()),
        'test_accuracy': float((y_pred == y_test).mean()),
    }
    meta_path = output_path.with_suffix('.json')
    with open(meta_path, 'w') as f:
        json.dump(meta, f, indent=2)
    print(f"Metadata saved to: {meta_path}")

    return output_path


def main():
    parser = argparse.ArgumentParser(description='Train field detection models')
    parser.add_argument('--field', type=str, default='all',
                        choices=['title', 'rating', 'genre', 'synopsis', 'all'],
                        help='Which field to train')
    parser.add_argument('--output-dir', type=str, default='../models', help='Output directory for ONNX files')
    parser.add_argument('--hidden-sizes', type=str, default='64,32', help='Hidden layer sizes')
    parser.add_argument('--epochs', type=int, default=300, help='Max iterations')
    parser.add_argument('--cv', type=int, default=0, help='Cross-validation folds (0=none)')
    args = parser.parse_args()

    lm_dir = Path(__file__).parent
    output_dir = lm_dir / args.output_dir
    output_dir.mkdir(exist_ok=True)

    hidden_sizes = tuple(int(s) for s in args.hidden_sizes.split(','))

    fields = ['title', 'rating', 'genre', 'synopsis'] if args.field == 'all' else [args.field]

    for field_name in fields:
        input_path = lm_dir / f'training_data_{field_name}.npz'
        if not input_path.exists():
            print(f"\nERROR: No training data for {field_name} at {input_path}")
            print(f"Run: python3 collect_training_data.py --field {field_name}")
            continue

        train_field(field_name, str(input_path), str(output_dir), hidden_sizes, args.epochs, args.cv)


if __name__ == '__main__':
    main()
