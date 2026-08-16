require 'json'

package = JSON.parse(File.read(File.join(__dir__, 'package.json')))

Pod::Spec.new do |s|
  s.name = 'NostosCapacitor'
  s.version = package['version']
  s.summary = package['description']
  s.description = package['description']
  s.license = package['license']
  s.homepage = package['homepage']
  s.author = 'Nostos contributors'
  # package.json repository urls carry a "git+" prefix; CocoaPods wants plain https.
  s.source = { :git => package['repository']['url'].sub('git+', ''), :tag => s.version.to_s }
  s.source_files = 'ios/Nostos/**/*.{swift,h,m,c,cc,mm,cpp}'
  # Capacitor 8 baseline (verified against @capacitor/ios 8.5.0, whose own
  # template and plugin podspecs pin 15.0).
  s.ios.deployment_target = '15.0'
  s.dependency 'Capacitor'
  s.swift_version = '5.9'
end
