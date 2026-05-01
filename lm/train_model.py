#!/usr/bin/env python3
"""Train unified multi-label field detection LM → export ONNX.

SINGLE MODEL approach:
  Input:  40-dim feature vector
  Output: 9 probabilities [title, rating, genre, synopsis, alt_title, author, status, similar, chapters]
  Architecture: 40 → 128 → 64 → 9 (sigmoid on each output)

The model is trained with multi-label binary cross-entropy loss.
sklearn MLPClassifier supports multi-label natively when y is 2D.

Usage:
  python3 train_model.py                   # train unified multi-label model
  python3 train_model.py --mode multilabel # same as above
  python3 train_model.py --mode per-field  # legacy per-field models
  python3 train_model.py --hidden-sizes 256,128,64
  python3 train_model.py --epochs 500
"""

import argparse
import json
import numpy as np
from pathlib import Path

from sklearn.neural_network import MLPClassifier
from sklearn.model_selection import cross_val_score, train_test_split
from sklearn.metrics import classification_report, confusion_matrix, multilabel_confusion_matrix
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
    # Bold text label patterns (32-34)
    'bold_contains_status', 'bold_contains_author', 'bold_contains_alternative',
    # Class/content patterns (35)
    'has_class_desc_or_synopsis',
    # Chapter link pattern (36)
    'href_contains_chapter',
    # Ancestor: mirip/bxcl containers (37)
    'is_inside_mirip_or_bxcl',
    # Class: lchx/series (38)
    'has_class_lchx_or_series',
    # Text contains "Chapter" (39)
    'text_contains_chapter',
]

NUM_FEATURES = len(FEATURE_NAMES)  # 40

# Field order — must match Rust FieldType enum
FIELD_NAMES = ['title', 'rating', 'genre', 'synopsis', 'alt_title', 'author', 'status', 'similar', 'chapters']
NUM_FIELDS = len(FIELD_NAMES)  # 9


def multilabel_mlp_to_onnx(clf, scaler, input_size, num_outputs):
    """Convert sklearn multi-label MLPClassifier to ONNX model.

    Architecture: Input → StandardScaler → MLP → Sigmoid → Output
    Output shape: [None, num_outputs]
    """
    nodes = []
    initializers = []

    input_tensor = helper.make_tensor_value_info(
        'features', TensorProto.FLOAT, [None, input_size])
    output_tensor = helper.make_tensor_value_info(
        'probabilities', TensorProto.FLOAT, [None, num_outputs])

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
    sigmoid_node = helper.make_node('Sigmoid', [prev_output], ['probabilities'], name='sigmoid')
    nodes.append(sigmoid_node)

    # Create graph
    graph = helper.make_graph(
        nodes, 'field_detector_multilabel',
        [input_tensor], [output_tensor], initializers)

    # Create model
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid('', 17)])
    model.ir_version = 8

    # Validate
    onnx.checker.check_model(model)

    return model


def train_multilabel(input_path, output_dir, hidden_sizes, epochs, cv):
    """Train unified multi-label model."""
    print(f"\n{'='*60}")
    print(f"  Training: UNIFIED MULTI-LABEL MODEL (9 fields)")
    print(f"  Fields: {FIELD_NAMES}")
    print(f"{'='*60}")

    # Load multi-label data
    data = np.load(input_path, allow_pickle=True)
    X = data['X']
    Y = data['Y']

    print(f"Loaded training data: {X.shape[0]} samples, {X.shape[1]} features")
    print(f"Label shape: {Y.shape}")

    # Adjust Y columns if needed (old data may have 4 columns)
    if Y.shape[1] < NUM_FIELDS:
        print(f"WARNING: Training data has {Y.shape[1]} fields, expected {NUM_FIELDS}")
        print(f"  Padding with zeros for missing fields...")
        padded = np.zeros((Y.shape[0], NUM_FIELDS), dtype=np.int32)
        padded[:, :Y.shape[1]] = Y
        Y = padded

    for i, field_name in enumerate(FIELD_NAMES):
        pos = int(Y[:, i].sum())
        total = Y.shape[0]
        print(f"  {field_name:12s}: {pos:5d} positive ({pos/total*100:.2f}%)")

    # Split train/test — stratify by any-positive indicator
    has_any_positive = Y.sum(axis=1) > 0
    X_train, X_test, Y_train, Y_test = train_test_split(
        X, Y, test_size=0.2, random_state=42, stratify=has_any_positive)

    print(f"\nTrain: {len(X_train)} | Test: {len(X_test)}")

    # StandardScaler
    scaler = StandardScaler()
    X_train_scaled = scaler.fit_transform(X_train)
    X_test_scaled = scaler.transform(X_test)

    print(f"\nModel: MLP ({X.shape[1]} → {' → '.join(str(s) for s in hidden_sizes)} → {NUM_FIELDS})")
    print(f"Max iterations: {epochs}")

    # Train multi-label MLPClassifier
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
    clf.fit(X_train_scaled, Y_train)

    # Evaluate per-field
    Y_pred = clf.predict(X_test_scaled)

    print("\n" + "=" * 60)
    print("EVALUATION ON TEST SET (PER-FIELD)")
    print("=" * 60)

    for i, field_name in enumerate(FIELD_NAMES):
        y_true = Y_test[:, i]
        y_pred = Y_pred[:, i]

        # Handle case where a field may have no positive samples in test
        if y_true.sum() == 0 and y_pred.sum() == 0:
            print(f"\n  --- {field_name} --- (no positive samples in test)")
            continue

        cm = confusion_matrix(y_true, y_pred)
        if cm.size == 1:
            # Only one class in test
            print(f"\n  --- {field_name} --- (single class in test)")
            print(f"  TN={cm[0][0]:5d}")
            continue

        tn, fp, fn, tp = cm.ravel()

        accuracy = (tn + tp) / (tn + fp + fn + tp)
        precision = tp / (tp + fp) if (tp + fp) > 0 else 0
        recall = tp / (tp + fn) if (tp + fn) > 0 else 0
        f1 = 2 * precision * recall / (precision + recall) if (precision + recall) > 0 else 0

        print(f"\n  --- {field_name} ---")
        print(f"  TN={tn:5d}  FP={fp:5d}")
        print(f"  FN={fn:5d}  TP={tp:5d}")
        print(f"  Accuracy:  {accuracy:.4f}")
        print(f"  Precision: {precision:.4f}")
        print(f"  Recall:    {recall:.4f}")
        print(f"  F1:        {f1:.4f}")

    # Overall accuracy (all labels correct)
    overall_acc = (Y_pred == Y_test).all(axis=1).mean()
    print(f"\n  Overall (all labels correct): {overall_acc:.4f}")

    # Feature importance (first layer weights)
    first_layer_weights = np.abs(clf.coefs_[0]).mean(axis=1)
    importance = sorted(zip(FEATURE_NAMES, first_layer_weights), key=lambda x: -x[1])
    print(f"\nFeature Importance (top 20):")
    for name, imp in importance[:20]:
        bar = '█' * int(imp * 50)
        print(f"  {name:35s} {imp:.4f} {bar}")

    # Export ONNX
    output_path = Path(output_dir) / 'field_detector.onnx'
    print(f"\nExporting to ONNX: {output_path}")

    # Refit scaler on ALL data
    scaler_full = StandardScaler()
    scaler_full.fit(X)

    onnx_model = multilabel_mlp_to_onnx(clf, scaler_full, X.shape[1], NUM_FIELDS)

    onnx.save(onnx_model, output_path)

    # Verify ONNX model
    import onnxruntime as ort
    sess = ort.InferenceSession(str(output_path))
    input_name = sess.get_inputs()[0].name
    output_name = sess.get_outputs()[0].name

    sample = X_test_scaled[:5].astype(np.float32)
    onnx_pred = sess.run([output_name], {input_name: sample})[0]
    sklearn_pred = clf.predict_proba(sample)

    # sklearn multi-label predict_proba returns list of arrays
    print(f"\nONNX verification (5 samples):")
    print(f"  ONNX output shape: {onnx_pred.shape}")
    print(f"  ONNX sample 0: {onnx_pred[0]}")

    if isinstance(sklearn_pred, list):
        # Multi-label: predict_proba returns list of (N, 2) arrays
        sklearn_probs = np.column_stack([p[:, 1] for p in sklearn_pred])
        print(f"  sklearn sample 0: {sklearn_probs[0]}")
        max_diff = np.abs(onnx_pred - sklearn_probs).max()
    else:
        print(f"  sklearn sample 0: {sklearn_pred[0]}")
        max_diff = np.abs(onnx_pred.flatten() - sklearn_pred.flatten()[:onnx_pred.size]).max()

    print(f"  max diff: {max_diff:.6f}")

    model_size = output_path.stat().st_size
    print(f"\nModel size: {model_size:,} bytes ({model_size/1024:.1f} KB)")

    # Save metadata
    meta = {
        'mode': 'multilabel',
        'field_names': FIELD_NAMES,
        'feature_names': FEATURE_NAMES,
        'num_features': NUM_FEATURES,
        'num_outputs': NUM_FIELDS,
        'hidden_sizes': list(hidden_sizes),
        'scaler_mean': scaler_full.mean_.tolist(),
        'scaler_scale': scaler_full.scale_.tolist(),
        'training_samples': int(len(X)),
        'field_positive_counts': {
            FIELD_NAMES[i]: int(Y[:, i].sum()) for i in range(NUM_FIELDS)
        },
        'test_overall_accuracy': float(overall_acc),
    }
    meta_path = output_path.with_suffix('.json')
    with open(meta_path, 'w') as f:
        json.dump(meta, f, indent=2)
    print(f"Metadata saved to: {meta_path}")

    return output_path


def train_field_legacy(field_name, input_path, output_dir, hidden_sizes, epochs, cv):
    """Legacy per-field training (backward compatible)."""
    print(f"\n{'='*60}")
    print(f"  Training: {field_name} (per-field mode)")
    print(f"{'='*60}")

    data = np.load(input_path, allow_pickle=True)
    X = data['X']
    y = data['y']

    print(f"Loaded training data: {X.shape[0]} samples, {X.shape[1]} features")
    print(f"  Positive ({field_name}) nodes: {y.sum()} ({y.sum()/len(y)*100:.2f}%)")

    X_train, X_test, y_train, y_test = train_test_split(
        X, y, test_size=0.2, random_state=42, stratify=y)

    print(f"\nTrain: {len(X_train)} | Test: {len(X_test)}")

    scaler = StandardScaler()
    X_train_scaled = scaler.fit_transform(X_train)
    X_test_scaled = scaler.transform(X_test)

    print(f"\nModel: MLP ({X.shape[1]} → {' → '.join(str(s) for s in hidden_sizes)} → 1)")

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

    y_pred = clf.predict(X_test_scaled)
    print(classification_report(y_test, y_pred, target_names=[f'non-{field_name}', field_name]))

    cm = confusion_matrix(y_test, y_pred)
    print(f"Confusion Matrix:")
    print(f"  TN={cm[0][0]:5d}  FP={cm[0][1]:5d}")
    print(f"  FN={cm[1][0]:5d}  TP={cm[1][1]:5d}")

    # Export ONNX (single output)
    output_path = Path(output_dir) / f'{field_name}_detector.onnx'
    print(f"\nExporting to ONNX: {output_path}")

    scaler_full = StandardScaler()
    scaler_full.fit(X)

    # Build single-output ONNX
    nodes = []
    initializers = []
    input_tensor = helper.make_tensor_value_info('features', TensorProto.FLOAT, [None, X.shape[1]])
    output_tensor = helper.make_tensor_value_info('probability', TensorProto.FLOAT, [None, 1])

    scale_init = numpy_helper.from_array(scaler_full.scale_.astype(np.float32), name='scaler_scale')
    mean_init = numpy_helper.from_array(scaler_full.mean_.astype(np.float32), name='scaler_mean')
    initializers.extend([scale_init, mean_init])
    nodes.append(helper.make_node('Sub', ['features', 'scaler_mean'], ['centered']))
    nodes.append(helper.make_node('Div', ['centered', 'scaler_scale'], ['normalized']))

    prev_output = 'normalized'
    for i, (weight, bias) in enumerate(zip(clf.coefs_, clf.intercepts_)):
        is_last = (i == len(clf.coefs_) - 1)
        w_name = f'layer{i}_weight'
        b_name = f'layer{i}_bias'
        initializers.extend([
            numpy_helper.from_array(weight.astype(np.float32), name=w_name),
            numpy_helper.from_array(bias.astype(np.float32), name=b_name),
        ])
        nodes.append(helper.make_node('MatMul', [prev_output, w_name], [f'layer{i}_mm']))
        nodes.append(helper.make_node('Add', [f'layer{i}_mm', b_name], [f'layer{i}_add']))
        if not is_last:
            nodes.append(helper.make_node('Relu', [f'layer{i}_add'], [f'layer{i}_act']))
            prev_output = f'layer{i}_act'
        else:
            prev_output = f'layer{i}_add'

    nodes.append(helper.make_node('Sigmoid', [prev_output], ['probability']))
    graph = helper.make_graph(nodes, f'{field_name}_detector', [input_tensor], [output_tensor], initializers)
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid('', 17)])
    model.ir_version = 8
    onnx.checker.check_model(model)
    onnx.save(model, output_path)

    meta = {
        'field': field_name, 'feature_names': FEATURE_NAMES,
        'num_features': NUM_FEATURES, 'hidden_sizes': list(hidden_sizes),
        'scaler_mean': scaler_full.mean_.tolist(), 'scaler_scale': scaler_full.scale_.tolist(),
        'training_samples': int(len(X)), 'positive_samples': int(y.sum()),
        'test_accuracy': float((y_pred == y_test).mean()),
    }
    with open(output_path.with_suffix('.json'), 'w') as f:
        json.dump(meta, f, indent=2)

    print(f"Model saved: {output_path} ({output_path.stat().st_size/1024:.1f} KB)")
    return output_path


def main():
    parser = argparse.ArgumentParser(description='Train field detection models')
    parser.add_argument('--mode', type=str, default='multilabel',
                        choices=['multilabel', 'per-field'],
                        help='Training mode: multilabel (unified) or per-field (legacy)')
    parser.add_argument('--field', type=str, default='all',
                        choices=FIELD_NAMES + ['all'],
                        help='Which field (per-field mode only)')
    parser.add_argument('--output-dir', type=str, default='../models',
                        help='Output directory for ONNX files')
    parser.add_argument('--hidden-sizes', type=str, default='256,128',
                        help='Hidden layer sizes (comma-separated)')
    parser.add_argument('--epochs', type=int, default=500,
                        help='Max iterations')
    parser.add_argument('--cv', type=int, default=0,
                        help='Cross-validation folds (0=none)')
    args = parser.parse_args()

    lm_dir = Path(__file__).parent
    output_dir = lm_dir / args.output_dir
    output_dir.mkdir(exist_ok=True)

    hidden_sizes = tuple(int(s) for s in args.hidden_sizes.split(','))

    if args.mode == 'multilabel':
        input_path = lm_dir / 'training_data_multilabel.npz'
        if not input_path.exists():
            print(f"\nERROR: No multi-label training data at {input_path}")
            print(f"Run: python3 collect_training_data.py --mode multilabel")
            return

        train_multilabel(
            str(input_path), str(output_dir),
            hidden_sizes, args.epochs, args.cv)

    else:
        # Legacy per-field mode
        fields = FIELD_NAMES if args.field == 'all' else [args.field]
        for field_name in fields:
            input_path = lm_dir / f'training_data_{field_name}.npz'
            if not input_path.exists():
                print(f"\nERROR: No training data for {field_name} at {input_path}")
                print(f"Run: python3 collect_training_data.py --mode per-field --field {field_name}")
                continue

            train_field_legacy(
                field_name, str(input_path), str(output_dir),
                hidden_sizes, args.epochs, args.cv)


if __name__ == '__main__':
    main()
