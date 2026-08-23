import 'dart:async';

import 'package:flutter/material.dart';
import 'package:multicast_dns/multicast_dns.dart';

import 'model.dart';

const String kServiceType = '_wdlink._udp.local';
const Duration kPtrWindow = Duration(seconds: 6);
const Duration kRecordTimeout = Duration(seconds: 2);

class DiscoveryPage extends StatefulWidget {
  const DiscoveryPage({super.key});

  @override
  State<DiscoveryPage> createState() => _DiscoveryPageState();
}

class _DiscoveryPageState extends State<DiscoveryPage> {
  bool _scanning = false;
  String _status = 'Starting…';
  List<DiscoveredReceiver> _receivers = const [];

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) => _scan());
  }

  Future<T?> _first<T extends ResourceRecord>(
    Stream<T> stream,
    Duration timeout,
  ) async {
    T? out;
    try {
      await for (final v
          in stream.timeout(timeout, onTimeout: (EventSink<T> s) => s.close())) {
        out = v;
        break;
      }
    } catch (_) {}
    return out;
  }

  Future<void> _scan() async {
    if (_scanning) return;
    setState(() {
      _scanning = true;
      _status = 'Scanning Wi-Fi for receivers…';
      _receivers = const [];
    });

    final found = <DiscoveredReceiver>[];
    final client = MDnsClient();
    try {
      await client.start();

      final names = <String>{};
      try {
        await for (final PtrResourceRecord ptr in client
            .lookup<PtrResourceRecord>(
                ResourceRecordQuery.serverPointer(kServiceType))
            .timeout(kPtrWindow,
                onTimeout: (EventSink<PtrResourceRecord> s) => s.close())) {
          names.add(ptr.domainName);
        }
      } catch (_) {}

      for (final fullName in names) {
        final srv = await _first<SrvResourceRecord>(
          client.lookup<SrvResourceRecord>(
              ResourceRecordQuery.service(fullName)),
          kRecordTimeout,
        );
        if (srv == null) continue;

        final addr = await _first<IPAddressResourceRecord>(
          client.lookup<IPAddressResourceRecord>(
              ResourceRecordQuery.addressIPv4(srv.target)),
          kRecordTimeout,
        );
        if (addr == null) continue;

        final txtRec = await _first<TxtResourceRecord>(
          client.lookup<TxtResourceRecord>(ResourceRecordQuery.text(fullName)),
          kRecordTimeout,
        );

        final txt = <String, String>{};
        for (final part in (txtRec?.text ?? '').split(RegExp(r'\s+'))) {
          final i = part.indexOf('=');
          if (i > 0) txt[part.substring(0, i)] = part.substring(i + 1);
        }

        var label = fullName;
        final suffix = '.$kServiceType';
        if (label.endsWith(suffix)) {
          label = label.substring(0, label.length - suffix.length);
        }

        found.add(DiscoveredReceiver(
          name: label,
          host: addr.address.address,
          port: srv.port,
          txt: txt,
        ));
      }
    } catch (e) {
      if (mounted) {
        setState(() => _status = 'Scan error: $e');
      }
    } finally {
      client.stop();
    }

    if (!mounted) return;
    setState(() {
      _scanning = false;
      _receivers = found;
      if (_status.startsWith('Scan error')) {
        // keep error text
      } else if (found.isEmpty) {
        _status =
            'No receivers found.\nMake sure the PC receiver is running and both devices are on the same Wi-Fi.';
      } else {
        _status = '${found.length} receiver(s) found';
      }
    });
  }

  void _showDetails(DiscoveredReceiver r) {
    showModalBottomSheet<void>(
      context: context,
      showDragHandle: true,
      builder: (ctx) => SafeArea(
        child: Padding(
          padding: const EdgeInsets.fromLTRB(24, 0, 24, 24),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(r.name,
                  style: Theme.of(ctx).textTheme.titleLarge),
              const SizedBox(height: 12),
              _kv('Address', '${r.host}:${r.port}'),
              _kv('Protocol', r.proto.isEmpty ? '(unknown)' : r.proto),
              _kv('Fingerprint',
                  r.fingerprint.isEmpty ? '(unknown)' : r.fingerprint),
              const SizedBox(height: 16),
              const Text(
                'Discovery works end-to-end. Secure pairing (SPAKE2 over QUIC) '
                'activates once the Rust core ships as a native library for '
                'Android — the next milestone.',
                style: TextStyle(color: Colors.white54, fontSize: 13),
              ),
            ],
          ),
        ),
      ),
    );
  }

  Widget _kv(String k, String v) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 4),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            SizedBox(
                width: 110,
                child: Text(k,
                    style: const TextStyle(color: Colors.white38))),
            Expanded(child: Text(v)),
          ],
        ),
      );

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('WD Cast'),
        actions: [
          IconButton(
            onPressed: _scanning ? null : _scan,
            icon: const Icon(Icons.refresh),
            tooltip: 'Rescan',
          ),
        ],
      ),
      body: RefreshIndicator(
        onRefresh: _scan,
        child: ListView(
          physics: const AlwaysScrollableScrollPhysics(),
          padding: const EdgeInsets.all(16),
          children: [
            Row(children: [
              if (_scanning)
                const Padding(
                  padding: EdgeInsets.only(right: 12),
                  child: SizedBox(
                    width: 18,
                    height: 18,
                    child: CircularProgressIndicator(strokeWidth: 2),
                  ),
                ),
              Expanded(
                child: Text(_status,
                    style: const TextStyle(color: Colors.white70)),
              ),
            ]),
            const SizedBox(height: 16),
            for (final r in _receivers)
              Card(
                margin: const EdgeInsets.only(bottom: 10),
                child: ListTile(
                  leading: const Icon(Icons.cast, size: 32),
                  title: Text(r.name),
                  subtitle: Text('${r.host}:${r.port}'
                      '${r.fingerprint.isEmpty ? '' : '\nfp ${r.fingerprint}'}'),
                  isThreeLine: r.fingerprint.isNotEmpty,
                  onTap: () => _showDetails(r),
                ),
              ),
            if (!_scanning && _receivers.isEmpty)
              const Padding(
                padding: EdgeInsets.only(top: 48),
                child: Center(
                  child: Icon(Icons.tv_off, size: 64, color: Colors.white24),
                ),
              ),
          ],
        ),
      ),
      floatingActionButton: FloatingActionButton.extended(
        onPressed: _scanning ? null : _scan,
        icon: const Icon(Icons.search),
        label: const Text('Scan'),
      ),
    );
  }
}
