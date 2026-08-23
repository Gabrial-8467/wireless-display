class DiscoveredReceiver {
  final String name;
  final String host;
  final int port;
  final Map<String, String> txt;

  const DiscoveredReceiver({
    required this.name,
    required this.host,
    required this.port,
    required this.txt,
  });

  String get fingerprint => txt['fp'] ?? '';
  String get proto => txt['proto'] ?? '';

  @override
  String toString() => '$name ($host:$port)';
}
