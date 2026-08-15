require 'json'

package = JSON.parse(File.read(File.join(__dir__, '..', 'package.json')))

Pod::Spec.new do |s|
  s.name = 'Nostos'
  s.module_name = 'Nostos'
  s.version = package['version']
  s.summary = package['description']
  s.description = package['description']
  s.license = package['license']
  s.homepage = package['homepage']
  s.author = 'Nostos contributors'
  s.source = { :git => package['repository']['url'], :tag => s.version.to_s }
  # Capacitor 8-era baseline (assumed — this pod has not been compiled here;
  # no Capacitor app project exists in-repo to `pod install` against).
  s.ios.deployment_target = '15.0'
  s.dependency 'Capacitor'
  s.swift_version = '5.9'
  s.source_files = 'Nostos/**/*.{swift,h,m,c,cpp,mm,metal}'
end
