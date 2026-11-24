import Darwin
import Testing

@main
struct AuditTestRunner {
    static func main() async {
        let code: CInt = await Testing.__swiftPMEntryPoint()
        exit(code)
    }
}
