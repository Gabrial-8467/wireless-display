import 'package:flutter/material.dart';

import 'discovery.dart';

void main() => runApp(const WDCastApp());

class WDCastApp extends StatelessWidget {
  const WDCastApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'WD Cast',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(
          seedColor: const Color(0xFF3D5AFE),
          brightness: Brightness.dark,
        ),
        useMaterial3: true,
      ),
      home: const DiscoveryPage(),
    );
  }
}
