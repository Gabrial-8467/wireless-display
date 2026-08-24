import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'model.dart';

const MethodChannel _ch = MethodChannel('wd_cast');
const EventChannel _events = EventChannel('wd_cast/events');

class CastPage extends StatefulWidget {
  final DiscoveredReceiver receiver;

  const CastPage({super.key, required this.receiver});

  @override
  State<CastPage> createState() => _CastPageState();
}

class _CastPageState extends State<CastPage> {
  String _status = 'idle';
  String? _error;
  bool _paired = false;
  int _sent = 0;
  int _dropped = 0;
  StreamSubscription<dynamic>? _eventSub;
  Timer? _poll;

  @override
  void initState() {
    super.initState();
    _setup();
  }

  Future<void> _setup() async {
    try {
      await _ch.invokeMethod('init');
      _paired = await _ch.invokeMethod('hasToken');
    } catch (_) {}
    _eventSub = _events.receiveBroadcastStream().listen((e) {
      final s = e.toString();
      if (s.startsWith('Streaming')) {
        setState(() => _status = 'streaming');
      } else if (s.startsWith('PermissionDenied')) {
        setState(() {
          _status = 'idle';
          _error = 'Screen capture permission denied';
        });
      } else if (s.startsWith('SessionRejected:')) {
        setState(() {
          _status = 'idle';
          _error = s.substring('SessionRejected:'.length);
        });
      }
    });
    _poll = Timer.periodic(const Duration(milliseconds: 600), (_) => _refresh());
  }

  Future<void> _refresh() async {
    if (!mounted) return;
    try {
      final s = await _ch.invokeMethod<String>('status');
      final m = (s != null) ? Uri.splitQueryString(s.replaceAll('&', '&').replaceAll(',', '&')) : <String, String>{};
      // status is JSON; parse minimally without adding deps.
      final state = _jsonStr(s ?? '', 'state');
      final err = _jsonStr(s ?? '', 'error');
      setState(() {
        _status = state.isEmpty ? _status : state;
        if (err.isNotEmpty) _error = err;
        _sent = int.tryParse(_jsonNum(s ?? '', 'sent')) ?? _sent;
        _dropped = int.tryParse(_jsonNum(s ?? '', 'dropped')) ?? _dropped;
        void unused(Map<String, String> _) {}
        unused(m);
      });
    } catch (_) {}
  }

  static String _jsonStr(String json, String key) {
    final re = RegExp('"$key"\\s*:\\s*"([^"]*)"');
    return re.firstMatch(json)?.group(1) ?? '';
  }

  static String _jsonNum(String json, String key) {
    final re = RegExp('"$key"\\s*:\\s*([0-9]+)');
    return re.firstMatch(json)?.group(1) ?? '';
  }

  Future<void> _pairFlow(BuildContext ctx) async {
    final codeCtrl = TextEditingController();
    final ok = await showDialog<bool>(
      context: ctx,
      builder: (ctx) => AlertDialog(
        title: const Text('Pair with this PC'),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(
                'A 6-digit PAIRING_CODE is shown on the PC\n(instance ${widget.receiver.name}).'),
            const SizedBox(height: 12),
            TextField(
              controller: codeCtrl,
              keyboardType: TextInputType.number,
              maxLength: 6,
              decoration: const InputDecoration(
                labelText: 'Pairing code',
                counterText: '',
              ),
            ),
          ],
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx, false),
              child: const Text('Cancel')),
          FilledButton(
              onPressed: () => Navigator.pop(ctx, true),
              child: const Text('Pair')),
        ],
      ),
    );
    if (ok != true || !mounted) return;
    setState(() { _status = 'pairing'; _error = null; });
    try {
      final r = await _ch.invokeMethod<String>('pair', {
        'host': widget.receiver.host,
        'port': widget.receiver.port,
        'code': codeCtrl.text.trim(),
        'name': 'Android Phone',
      });
      final okFlag = _jsonStr(r ?? '', 'ok') == 'true';
      final err = _jsonStr(r ?? '', 'error');
      setState(() {
        if (okFlag) {
          _paired = true;
          _status = 'idle';
        } else {
          _status = 'idle';
          _error = err.isEmpty ? 'pairing failed' : err;
        }
      });
    } catch (e) {
      setState(() { _status = 'idle'; _error = '$e'; });
    }
  }

  Future<void> _startCast() async {
    setState(() { _error = null; _status = 'connecting'; });
    try {
      await _ch.invokeMethod('startCast', {
        'host': widget.receiver.host,
        'port': widget.receiver.port,
      });
      // Actual streaming state arrives via service events / polling.
    } catch (e) {
      setState(() { _status = 'idle'; _error = '$e'; });
    }
  }

  Future<void> _stopCast() async {
    try {
      await _ch.invokeMethod('stopCast');
    } catch (_) {}
    setState(() => _status = 'stopped');
  }

  @override
  Widget build(BuildContext context) {
    final casting = _status == 'streaming' || _status == 'connecting' ||
        _status == 'offering';
    return Scaffold(
      appBar: AppBar(
        title: Text(widget.receiver.name,
            style: const TextStyle(fontSize: 18)),
      ),
      body: Padding(
        padding: const EdgeInsets.all(20),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Card(
              child: Padding(
                padding: const EdgeInsets.all(16),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text('${widget.receiver.host}:${widget.receiver.port}'),
                    if (widget.receiver.fingerprint.isNotEmpty)
                      Text('fp ${widget.receiver.fingerprint}',
                          style: const TextStyle(color: Colors.white38)),
                  ],
                ),
              ),
            ),
            const SizedBox(height: 16),
            Row(children: [
              Icon(casting ? Icons.cast_connected : Icons.cast,
                  color: casting ? Colors.greenAccent : null),
              const SizedBox(width: 8),
              Text('Status: $_status',
                  style: Theme.of(context).textTheme.titleMedium),
            ]),
            if (_error != null) ...[
              const SizedBox(height: 8),
              Text(_error!,
                  style: const TextStyle(color: Colors.redAccent)),
            ],
            const Spacer(),
            if (!_paired)
              FilledButton.icon(
                onPressed: _status == 'pairing'
                    ? null
                    : () => _pairFlow(context),
                icon: const Icon(Icons.link),
                label: const Text('Pair with PC (enter code)'),
              )
            else ...[
              FilledButton.icon(
                onPressed:
                    casting ? null : _startCast,
                icon: const Icon(Icons.screen_share),
                label: const Text('Start casting screen'),
              ),
              const SizedBox(height: 10),
              OutlinedButton.icon(
                onPressed: casting ? _stopCast : null,
                icon: const Icon(Icons.stop),
                label: const Text('Stop'),
              ),
            ],
            const SizedBox(height: 8),
            Text('packets sent $_sent · dropped $_dropped',
                textAlign: TextAlign.center,
                style: const TextStyle(color: Colors.white24)),
          ],
        ),
      ),
    );
  }

  @override
  void dispose() {
    _poll?.cancel();
    _eventSub?.cancel();
    super.dispose();
  }
}
