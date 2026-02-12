//! ALPN helpers shared by client, server, and discovery modules.

/// Generate ALPN protocol bytes from a tonic service name.
///
/// Converts `echo.Echo` -> `/echo.Echo/1.0`.
/// Converts `p2p_chat.P2PChatService` -> `/p2p_chat.P2PChatService/1.0`.
pub fn service_to_alpn<T: tonic::server::NamedService>() -> Vec<u8> {
    let service_name = T::NAME;
    format!("/{service_name}/1.0").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockService;
    impl tonic::server::NamedService for MockService {
        const NAME: &'static str = "test.Service";
    }

    #[test]
    fn test_alpn_generation() {
        assert_eq!(service_to_alpn::<MockService>(), b"/test.Service/1.0");

        struct EchoService;
        impl tonic::server::NamedService for EchoService {
            const NAME: &'static str = "echo.Echo";
        }

        struct ChatService;
        impl tonic::server::NamedService for ChatService {
            const NAME: &'static str = "p2p_chat.P2PChatService";
        }

        assert_eq!(service_to_alpn::<EchoService>(), b"/echo.Echo/1.0");
        assert_eq!(
            service_to_alpn::<ChatService>(),
            b"/p2p_chat.P2PChatService/1.0"
        );
    }
}
