// Standard integration_test driver:
// flutter drive --driver=test_driver/integration_test.dart \
//   --target=integration_test/<smoke>.dart -d <device> --release
import 'package:integration_test/integration_test_driver.dart';

Future<void> main() => integrationDriver();
